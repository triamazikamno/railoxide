use super::super::*;
use super::*;
use crate::root::chain_load::{ChainUtxoState, WalletSyncLifecycle};
use alloy::primitives::B256;
use broadcaster_core::contracts::swap_math::{SWAP_MATH_ADDRESS, SWAP_MATH_CREATION_CODE};
use gpui::{IntoElement, ParentElement, Render, Styled, TestAppContext, div};
use gpui_component::{Root, WindowExt};
use std::cell::Cell;
use std::rc::Rc;
use wallet_ops::vault::ExecutorStore;

/// Mount the root as an entity, as startup does. Calling its render helper through
/// `read` would miss attempts to read the root while GPUI holds its update lease.
struct WalletWindow(Entity<WalletRoot>);

impl Render for WalletWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone()).children(
            crate::root::startup::render_wallet_overlay_layers(window, cx),
        )
    }
}

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
            window.open_dialog(cx, move |dialog, _, cx| {
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
            window.open_dialog(cx, move |dialog, _, cx| {
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

#[gpui::test]
fn quote_retry_discards_the_old_route_and_ignores_its_late_response(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, _, operation, runtime, cx| {
        let owner = swaps.read_with(cx, |swaps, _| Arc::clone(swaps.private_owner().unwrap()));
        let old_client = runtime.block_on(owner.swap_orderbook_client()).unwrap();
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
            window.open_dialog(cx, move |dialog, _, _| {
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
        executors.retire(operation).unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let stage = swaps.stage(swaps.record(operation).unwrap());
                assert_eq!(stage, model::SwapStage::SetupRetired);
                let actions = model::swap_actions(stage, false);
                assert!(actions.resume.is_some_and(|available| available.is_ok()));
                // Ended, it leaves the Private tab by itself.
                assert!(actions.recover && !actions.dismiss);
                swaps.open_existing_form(operation, window, cx);
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

/// A submitted setup whose broadcaster has not confirmed it. Its fee input must
/// survive a retry, stop, and restart because the signed payload can still execute.
fn pending_setup(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
) -> wallet_ops::vault::ExecutorRecord {
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorInputIdentity;
    let record = executors
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    executors
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16197, "commitment": "0x1"
    }))
    .unwrap();
    record_setup(
        executors,
        operation,
        record.delegate(),
        B256::repeat_byte(4),
        vec![input],
    )
}

fn record_setup(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delegate: Address,
    payload_hash: alloy::primitives::B256,
    inputs: Vec<wallet_ops::vault::ExecutorInputIdentity>,
) -> wallet_ops::vault::ExecutorRecord {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload,
    };
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    executors.record_account_read(operation, observed).unwrap();
    executors
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                payload_hash,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), observed, inputs),
            ),
        )
        .unwrap()
}

#[gpui::test]
fn setup_confirmation_tracks_local_inclusion_without_advancing_the_swap(cx: &mut TestAppContext) {
    use crate::root::utxo::UtxoFinalityContext;
    use alloy::primitives::B256;
    with_swap_view(cx, |_, _, executors, operation, _, _| {
        let record = pending_setup(executors, operation);
        let transaction = B256::repeat_byte(80);
        let record = executors
            .record_submission(operation, record.issued()[0].hash(), transaction)
            .unwrap();
        let mut utxo = wallet_ops::UtxoOutput {
            tree: 0,
            position: 0,
            token: String::new(),
            value: "1".into(),
            commitment_kind: "Transact".into(),
            activity_classification: "Private Output".into(),
            blocked_shield_rescue: None,
            commitment: String::new(),
            npk: String::new(),
            blinded_commitment: String::new(),
            poi_statuses: BTreeMap::new(),
            ppoi_state: wallet_ops::UtxoPpoiState::Unknown,
            ppoi_last_submission_at: None,
            poi_spendable: false,
            source_tx_hash: B256::repeat_byte(90).to_string(),
            source_block_number: 90,
            source_block_timestamp: 0,
            is_spent: false,
            pending_new: false,
            pending_spent: true,
            local_pending_spent: false,
            spent_tx_hash: Some(transaction.to_string()),
            spent_block_number: Some(100),
        };
        // The spend is already visible in private sync, but not yet safe.
        for (head, expected) in [
            (100, "Confirming (0/12 blocks)"),
            (105, "Confirming (5/12 blocks)"),
            (112, "Verifying setup…"),
        ] {
            assert_eq!(
                model::swap_setup_confirmation(&record, std::slice::from_ref(&utxo))
                    .and_then(|confirmation| confirmation.detail(UtxoFinalityContext::new(
                        Some(head),
                        Some(head - 12),
                        Some(12),
                    )))
                    .as_deref(),
                Some(expected)
            );
            assert_eq!(
                swap_stage(&record, None, false, None),
                SwapStage::SetupPending
            );
        }
        // If private sync rolls the inclusion back, an unrelated transaction must
        // not leave the setup showing a stale confirmation count.
        utxo.spent_tx_hash = None;
        utxo.spent_block_number = None;
        let finality = UtxoFinalityContext::new(Some(105), Some(93), Some(12));
        assert!(model::swap_setup_confirmation(&record, std::slice::from_ref(&utxo)).is_none());
        // Received change can provide the same hint even if the spent note is absent.
        utxo.source_tx_hash = transaction.to_string();
        utxo.source_block_number = 100;
        assert_eq!(
            model::swap_setup_confirmation(&record, &[utxo])
                .and_then(|confirmation| confirmation.detail(finality))
                .as_deref(),
            Some("Confirming (5/12 blocks)")
        );
    });
}

#[gpui::test]
fn stale_setup_read_is_dropped_and_a_failed_read_records_its_error(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                // A setup's read pages through no history, so it has no range.
                let result = |outcome| {
                    vec![ObservationResult {
                        operation,
                        range_end: None,
                        outcome,
                        destination: None,
                    }]
                };
                // Either account's background read can lose a race with confirmation.
                // Its stale result must neither show a review error nor advance tracking.
                for destination in [None, Some(operation)] {
                    let error = eyre::Report::from(wallet_ops::ExecutorRecordChanged)
                        .wrap_err("checking setup");
                    let mut stale = result(Err(error));
                    stale[0].destination = destination;
                    assert!(swaps.apply_observations(stale, window, cx).is_empty());
                    assert!(swaps.tracking.get(&operation).is_none_or(|tracking| {
                        tracking.error.is_none()
                            && tracking.setup.is_none()
                            && tracking.destination_setup.is_none()
                    }));
                }
                assert!(
                    swaps
                        .apply_observations(
                            result(Err(eyre::eyre!("RPC unavailable"))),
                            window,
                            cx,
                        )
                        .is_empty(),
                    "failed reads wait before retrying"
                );
                assert!(swaps.tracking.get(&operation).unwrap().error.is_some());
            });
        });
    });
}

#[gpui::test]
fn pending_setup_can_retry_and_stop_without_losing_its_reservation(cx: &mut TestAppContext) {
    use alloy::eips::{BlockNumHash, eip7702::constants::EIP7702_DELEGATION_DESIGNATOR};
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let issued = pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        // Hiding an account is only presentation; it must not stop its pending swap.
        executors.set_hidden(operation, true).unwrap();
        let observed = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            swaps
                .next_observations(cx)
                .is_some_and(|(_, _, pages)| pages.iter().any(|page| page.operation == operation))
        };
        // A setup unconfirmed for long is still read on every pass.
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(1_000);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(observed(swaps, cx));
            });
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(swaps.has_shown_swaps());
                assert!(!swaps.record(operation).unwrap().is_swap_setup_stopped());
                swaps.tracking.entry(operation).or_default().auto_place = true;
                assert_eq!(
                    swaps.stage(swaps.record(operation).unwrap()),
                    SwapStage::SetupPending
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let retry = cx.debug_bounds("swap-progress-continue").unwrap();
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.operation, Some(operation));
                assert_eq!(swaps.form_mode(form), FormMode::Setup { resume: true });
                assert!(!swaps.tracking.get(&operation).unwrap().auto_place);
                let preview = swaps
                    .private_owner()
                    .unwrap()
                    .swap_setup_preview(operation)
                    .unwrap();
                assert_eq!(preview.executor(), issued.address().unwrap());
                assert!(preview.delegated().is_none());
                swaps.form = None;
                // A local delivery may still be waiting when the user stops it.
                let join = runtime.spawn(std::future::pending::<()>());
                swaps.job = Some(SwapJob {
                    operation,
                    kind: SwapJobKind::Setup,
                    abort: join.abort_handle(),
                });
                swaps.tracking.get_mut(&operation).unwrap().auto_place = true;
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let stop = cx.debug_bounds("swap-progress-cancel-preparation").unwrap();
        cx.simulate_click(stop.center(), gpui::Modifiers::none());
        // Confirm the alert through its normal keyboard action.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|_, cx| {
            let swaps = swaps.read(cx);
            let record = swaps.record(operation).unwrap();
            assert!(record.is_swap_setup_stopped());
            assert!(!swaps.has_shown_swaps());
            assert!(swaps.job.is_none());
            assert!(!swaps.tracking.get(&operation).unwrap().auto_place);
            assert_eq!(record.reserved_inputs(), issued.reserved_inputs());
        });
        // Nothing waits on a stopped setup, so the swap view stops reading its account.
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                assert!(!observed(swaps, cx));
            });
        });
        // Unhiding the account cannot undo the persisted stop, even if setup confirms later.
        executors.set_hidden(operation, false).unwrap();
        assert!(
            executors
                .records()
                .unwrap()
                .iter()
                .find(|record| record.operation() == operation)
                .unwrap()
                .is_swap_setup_stopped()
        );
        assert!(
            executors
                .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
                .is_err()
        );
        let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
        let record = executors
            .record_account_read(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::ONE),
            )
            .unwrap();
        let profile = root.read_with(cx, |root, _| {
            root.effective_chain_configs
                .get(1)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
        });
        let code = [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            profile.delegate().as_slice(),
        ]
        .concat();
        let setup = wallet_ops::swap_setup_status(&record, confirmed, &code, profile);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.setup = Some(setup);
                tracking.auto_place = true;
                assert_eq!(
                    swaps.stage(swaps.record(operation).unwrap()),
                    SwapStage::Approved
                );
                assert!(!swaps.has_shown_swaps());
                swaps.continue_approved_swaps(window, cx);
                assert!(swaps.pending_authorization.is_none());
                assert!(swaps.job.is_none());
            });
        });
    });
}

#[gpui::test]
fn setup_whose_nonce_is_recorded_as_consumed_is_approved_and_needs_no_setup_page(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), test_approval())
            .unwrap();
        let observed = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            swaps
                .next_observations(cx)
                .is_some_and(|(_, _, pages)| pages.iter().any(|page| page.operation == operation))
        };
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(100);
            });
            swaps.update(cx, |swaps, _| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().setup =
                    Some(wallet_ops::SwapSetupStatus::Pending);
            });
        });
        // The account's nonce is read past the setup. The swap is ready to place its approved
        // order without another account read, also once that read's observation is
        // invalidated.
        let consumed =
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
        executors.record_account_read(operation, consumed).unwrap();
        executors.invalidate_observation(operation).unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert!(record.nonce_observation().is_none());
                assert_eq!(
                    swaps.tracking.get(&operation).unwrap().setup,
                    Some(wallet_ops::SwapSetupStatus::Pending)
                );
                assert_eq!(swaps.stage(record), SwapStage::Approved);
                assert!(
                    !observed(swaps, cx),
                    "a setup recorded as resolved needs no setup page"
                );
            });
        });
    });
}

#[gpui::test]
fn dismissed_expired_swap_stays_dormant_after_restart(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        ExecutorInputIdentity, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload, SwapAttempt, SwapDelivery, SwapObservation,
        SwapOrderObservations, SwapPreHookDeath, SwapPreHookDeathCause, SwapProof, SwapRecipient,
        SwapTerms,
    };
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let setup = pending_setup(executors, operation);
        let setup_hash = setup.issued()[0].hash();
        let observed =
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
        executors.record_account_read(operation, observed).unwrap();
        let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
            "tree": 4, "position": 16198, "commitment": "0x2"
        }))
        .unwrap();
        let uid = OrderUid::new(B256::repeat_byte(0x11), Address::repeat_byte(3), 20);
        let hook = |nonce, hash, purpose, inputs| {
            IssuedExecutorPayload::new(
                U256::from(nonce),
                setup.delegate(),
                B256::repeat_byte(hash),
                purpose,
                ExecutorPayloadContext::new(Bytes::from_static(b"hook"), observed, inputs),
            )
        };
        executors
            .record_swap_attempt(
                operation,
                SwapAttempt {
                    use_id: wallet_ops::vault::SwapUseId::first(operation),
                    terms: SwapTerms::new(
                        Address::repeat_byte(1),
                        Address::repeat_byte(2),
                        SwapRecipient::new(U256::ONE, [7; 32]),
                        setup_hash,
                    ),
                    proof: SwapProof::new(B256::repeat_byte(6), vec![input.clone()]),
                    uid,
                    submission: None,
                    delivery: SwapDelivery::Reshield,
                    bounds: test_approval().bounds,
                    invalidates: None,
                    pre_hook: hook(
                        1_u64,
                        7,
                        ExecutorPayloadPurpose::SwapPreHook,
                        vec![input.clone()],
                    ),
                    post_hook: Some(hook(
                        2_u64,
                        8,
                        ExecutorPayloadPurpose::SwapPostHook,
                        Vec::new(),
                    )),
                    bridge: None,
                },
            )
            .unwrap();
        let record = executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    pre_hook_dead: Some(SwapPreHookDeath {
                        cause: SwapPreHookDeathCause::Expired,
                        observation: SwapObservation {
                            block: observed.block(),
                            transaction_hash: None,
                        },
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!record.reserved_inputs().contains(&input));
        executors.set_hidden(operation, true).unwrap();
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(100);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert_eq!(record.nonce_observation(), Some(observed));
                assert!(!record.reserved_inputs().contains(&input));
                assert!(!swaps.has_shown_swaps());
                assert!(
                    swaps.next_observations(cx).is_none(),
                    "completed swaps must not trigger account-specific RPC after restart"
                );
                // Removed from the Private tab, the swap stays in My orders.
                swaps.show_view(dialog::SwapDialogView::Orders, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let row = cx
            .debug_bounds(format!("swap-order-row-{}", operation.opaque_id()).leak())
            .expect("a removed swap stays listed in My orders");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            assert_eq!(
                swaps.read(cx).dialog.as_ref().map(|dialog| dialog.view),
                Some(dialog::SwapDialogView::Detail(operation)),
                "selecting it opens its detail"
            );
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("swap-progress-remove").is_none(),
            "an already removed swap isn't offered for removal again"
        );

        // Older records may have lost their nonce evidence during an interrupted check.
        // Keep those notes locked, but don't turn startup into a migration RPC sweep.
        executors.invalidate_observation(operation).unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(
                    swaps
                        .record(operation)
                        .unwrap()
                        .reserved_inputs()
                        .contains(&input)
                );
                assert!(swaps.next_observations(cx).is_none());
            });
        });

        let pending = ExecutorOperationId::random().unwrap();
        executors
            .reserve(pending, setup.delegate(), Some("Private swap"), &[])
            .unwrap();
        executors
            .bind_address(pending, Address::repeat_byte(9))
            .unwrap();
        let pending_nonce = ExecutorNonceObservation::new(observed.block(), U256::ZERO);
        executors
            .record_account_read(pending, pending_nonce)
            .unwrap();
        executors
            .record_issued(
                pending,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    setup.delegate(),
                    B256::repeat_byte(9),
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"setup"),
                        pending_nonce,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let (_, _, pages) = swaps.next_observations(cx).expect("resume pending setup");
                assert_eq!(pages.len(), 1);
                assert_eq!(pages[0].operation, pending);
            });
        });
    });
}

#[gpui::test]
fn private_tab_details_opens_the_swap_or_the_open_orders(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_details(window, cx);
                assert_eq!(
                    swaps.dialog.as_ref().map(|dialog| dialog.view),
                    Some(dialog::SwapDialogView::Detail(operation))
                );
            });
            window.draw(cx).clear(cx);
        });
        // With several swaps on the Private tab, Details… lists the open ones.
        let second = ExecutorOperationId::random().unwrap();
        let delegate = root.read_with(cx, |root, _| {
            root.effective_chain_configs
                .get(1)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
                .delegate()
        });
        executors
            .reserve(
                second,
                delegate,
                Some("Private swap"),
                &[
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(1)),
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(2)),
                ],
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.open_details(window, cx);
                assert_eq!(
                    swaps.dialog.as_ref().map(|dialog| dialog.view),
                    Some(dialog::SwapDialogView::Orders)
                );
                assert_eq!(swaps.orders_filter, Some(model::SwapOrderGroup::Open));
            });
            window.draw(cx).clear(cx);
        });
        for shown in [operation, second] {
            assert!(
                cx.debug_bounds(format!("swap-order-row-{}", shown.opaque_id()).leak())
                    .is_some()
            );
        }
    });
}

fn test_approval() -> SwapApproval {
    SwapApproval {
        bounds: wallet_ops::vault::SwapApprovedBounds {
            sell_amount: U256::from(100),
            unshield_amount: None,
            unshield_fee_bps: U256::ZERO,
            buy_amount: U256::from(99),
            private_minimum: U256::from(98),
            shield_fee_bps: U256::from(25),
            slippage_bps: 50,
            pre_hook_gas_limit: 1_000_000,
            post_hook_gas_limit: Some(1_000_000),
            hook_cost: Some(U256::ONE),
            anchors: Vec::new(),
            destination_minimum: None,
            gas_share_bps: None,
            gas_estimate: None,
            gas_allowance: None,
            gas_price_wei: None,
            valid_for_secs: None,
            destination_shield_fee_bps: None,
            delivery_allowance: None,
            destination_setup_fee: None,
            source_setup_fee: None,
        },
        price_verified: Some(false),
        price_acknowledged: true,
        delivery: wallet_ops::vault::SwapDelivery::Reshield,
        tokens: None,
        accounts: None,
    }
}

fn with_swap_view(
    cx: &mut TestAppContext,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &mut gpui::VisualTestContext,
    ),
) {
    with_swap_view_and_rpc(cx, None, test);
}

/// [`with_swap_view`] with the session's chain RPC at `rpc`, such as [`SwapStubs::rpc`],
/// instead of an unreachable one.
fn with_swap_view_and_rpc(
    cx: &mut TestAppContext,
    rpc: Option<reqwest::Url>,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &mut gpui::VisualTestContext,
    ),
) {
    with_swap_view_and_store(
        cx,
        rpc,
        |root, swaps, executors, operation, runtime, _, cx| {
            test(root, swaps, executors, operation, runtime, cx);
        },
    );
}

fn with_swap_view_and_store(
    cx: &mut TestAppContext,
    rpc: Option<reqwest::Url>,
    test: impl FnOnce(
        &Entity<WalletRoot>,
        &Entity<PrivateSwapsView>,
        &ExecutorStore,
        ExecutorOperationId,
        &tokio::runtime::Runtime,
        &wallet_ops::WalletSessionStore,
        &mut gpui::VisualTestContext,
    ),
) {
    // Parallel fixtures can receive the same wall-clock timestamp. Reserve the
    // directory atomically before opening its database.
    let directory = tempfile::Builder::new()
        .prefix("swap-ui-")
        .tempdir()
        .unwrap();
    let path = directory.path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(|cx| {
        gpui_component::init(cx);
        ui::theme::apply_zenburn_component_theme(cx);
        // Dialogs slide in over a wall-clock animation, and a test build redraws a dirty
        // window between a click's mouse down and up. Mid-animation, that redraw moves the
        // dialog, so the release can miss the element the click measured. With reduced
        // motion, a dialog settles on its first frame, as gpui-component's own tests do.
        cx.set_reduce_motion(true);
    });
    let mut fixture = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let root = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        let view = root.read(cx).view_session.clone().unwrap();
        let vault = root.read(cx).vault_store.clone().unwrap();
        let mut chain = root
            .read(cx)
            .effective_chain_configs
            .get(1)
            .unwrap()
            .clone();
        chain.rpc_route = wallet_ops::RpcChainRoute::new(
            1,
            vec![rpc.unwrap_or_else(|| "http://127.0.0.1:1".parse::<reqwest::Url>().unwrap())],
        );
        let private = chain.railgun.as_mut().unwrap();
        private.archive_rpc_url = None;
        private.sync.quick_sync_endpoint = None;
        private.sync.indexed_artifact_source = None;
        let poi = wallet_ops::PoiReadSource::PoiProxy {
            rpc_url: "http://127.0.0.1:1".parse::<reqwest::Url>().unwrap().into(),
        };
        let store = wallet_ops::WalletSessionStore::from_db(vault.db(), poi.clone()).unwrap();
        let mut lifecycle = WalletSyncLifecycle::new();
        let registration = lifecycle.prepare_startup(1);
        let session = runtime.block_on(async {
            let http = wallet_ops::build_wallet_network_context(wallet_ops::WalletNetworkConfig {
                network_mode: Some(wallet_ops::WalletNetworkMode::Direct),
                proxy: None,
                data_dir: &path,
            })
            .await
            .unwrap();
            Box::pin(store.start_view_wallet_session_immediate(
                wallet_ops::ViewWalletChainSessionRequest {
                    view_session: view.clone(),
                    wallet_scope_generation: registration.generation,
                    chain_id: 1,
                    effective_chain: chain.clone(),
                    sync_start_policy:
                        wallet_ops::DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
                    init_block_number: Some(0),
                    sync_to_block: Some(0),
                    use_indexed_wallet_catch_up: false,
                    poi_read_source: poi,
                    rewind_wallet_cache: false,
                    progress_tx: None,
                },
                &http,
            ))
            .await
            .unwrap()
        });
        let session = Arc::new(session);
        let operation = ExecutorOperationId::random().unwrap();
        let executors = ExecutorStore::new(vault.db(), view, 1).unwrap();
        executors
            .reserve(
                operation,
                chain.accepted_executor_profile().unwrap().delegate(),
                Some("Private swap"),
                &[
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(1)),
                    wallet_ops::ExecutorAsset::Erc20(Address::repeat_byte(2)),
                ],
            )
            .unwrap();
        let observation = session.observation_rx.borrow().clone();
        root.update(cx, |root, _| {
            root.chain_states.insert(
                1,
                ChainUtxoState::Ready {
                    snapshot: observation.snapshot,
                    session: session.clone(),
                    observer_token: registration.observer_token,
                    sync_tip: wallet_ops::WalletSyncTip::default(),
                    poi_refreshing: false,
                    ppoi_workflow_status: observation.ppoi_workflow_status,
                },
            );
        });
        fixture = Some((root.clone(), store, session, executors, operation));
        let view = cx.new(|_| WalletWindow(root));
        Root::new(view, window, cx)
    });
    let (root, store, session, executors, operation) = fixture.unwrap();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let swaps = root.read_with(cx, |root, _| root.private_swaps_view().unwrap());
    assert!(swaps.read_with(cx, |swaps, _| swaps.has_shown_swaps()));

    test(&root, &swaps, &executors, operation, &runtime, &store, cx);
    cx.update(|window, _| window.remove_window());
    drop(swaps);
    drop(host);
    drop(root);
    cx.run_until_parked();
    runtime.block_on(async {
        session.stop().await.unwrap();
        store.shutdown().await;
    });
    drop(session);
    drop(store);
    drop(executors);
    drop(entered);
    drop(runtime);
    directory.close().unwrap();
}

#[gpui::test]
fn place_order_keeps_progress_visible_while_checking_terms_and_after_failure(
    cx: &mut TestAppContext,
) {
    with_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
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
        let code = [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            profile.delegate().as_slice(),
        ]
        .concat();
        let setup = wallet_ops::swap_setup_status(&record, confirmed, &code, profile);
        let approval = test_approval();
        executors
            .record_swap_approval(operation, SwapUseId::first(operation), approval.clone())
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().setup = Some(setup);
                assert_eq!(
                    swaps.stage(swaps.record(operation).unwrap()),
                    SwapStage::Approved
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let button = cx.debug_bounds("swap-progress-continue").unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "progress stays visible while getting the quote"
            );
            swaps.update(cx, |swaps, cx| {
                let job = swaps
                    .job
                    .take()
                    .expect("recorded setup can start a quote without a freshly synced chain head");
                assert_eq!(job.kind, SwapJobKind::Requote);
                // Replace the network work with a controlled response. Its cancelled
                // completion must not overwrite the next retry's state.
                job.abort.abort();
                swaps.job_revision += 1;
                swaps.apply_approved_quote(
                    operation,
                    SwapUseId::first(operation),
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        bridge_clients: None,
                        bridge: None,
                        outcome: Err(eyre::eyre!("initial quote unavailable")),
                    },
                    window,
                    cx,
                );
                assert!(swaps.tracking.get(&operation).unwrap().error.is_some());
            });
            assert!(
                window.has_active_dialog(cx),
                "a quote failure stays visible"
            );
        });

        // Hold the quote response to exercise an arbitrarily slow request without using
        // live services or a timer. Starting a retry clears the previous failure.
        let (send, receive) = tokio::sync::oneshot::channel();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let approval = approval.clone();
                swaps.start_job(
                    operation,
                    SwapJobKind::Requote,
                    async move { Ok(receive.await.unwrap()) },
                    move |swaps, result, window, cx| {
                        swaps.apply_approved_quote(
                            operation,
                            SwapUseId::first(operation),
                            &approval,
                            result,
                            window,
                            cx,
                        );
                    },
                    window,
                    cx,
                );
                assert!(swaps.tracking.get(&operation).unwrap().error.is_none());
            });
            window.draw(cx).clear(cx);
            assert!(window.has_active_dialog(cx));
        });
        send.send(QuoteResult {
            orderbook: None,
            bridge_clients: None,
            bridge: None,
            outcome: Err(eyre::eyre!("quote unavailable")),
        })
        .ok()
        .unwrap();
        runtime.block_on(tokio::task::yield_now());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let swaps = swaps.read(cx);
            assert!(!swaps.busy());
            assert_eq!(
                swaps.tracking.get(&operation).unwrap().error.as_deref(),
                Some("quote unavailable")
            );
            assert!(
                window.has_active_dialog(cx),
                "a failed quote must leave progress open for retry"
            );
        });

        // Another dialog can open while the request is in flight. It must keep focus
        // and remain open, even when this swap's progress is still underneath it.
        cx.update(|window, cx| {
            window.open_dialog(cx, |dialog, _, _| dialog.title("Unrelated form"));
            let focus = window.focused(cx);
            swaps.update(cx, |swaps, cx| {
                swaps.apply_approved_quote(
                    operation,
                    SwapUseId::first(operation),
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        bridge_clients: None,
                        bridge: None,
                        outcome: Ok(QuoteOutcome::PriceBlocked(PriceBlock::Deviates)),
                    },
                    window,
                    cx,
                );
                assert!(swaps.form.is_none());
                assert!(swaps.tracking.get(&operation).unwrap().auto_place);
                // A failed response still records its error and stops automatic retries.
                swaps.apply_approved_quote(
                    operation,
                    SwapUseId::first(operation),
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        bridge_clients: None,
                        bridge: None,
                        outcome: Err(eyre::eyre!("quote failed again")),
                    },
                    window,
                    cx,
                );
                let tracking = swaps.tracking.get(&operation).unwrap();
                assert!(!tracking.auto_place);
                assert_eq!(tracking.error.as_deref(), Some("quote failed again"));
            });
            assert_eq!(window.focused(cx), focus);
            window.close_dialog(cx);
            window.draw(cx).clear(cx);
        });

        // A successful quote that changes the approved terms must advance from this
        // swap's detail to review, rather than defer forever because a dialog is open.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.apply_approved_quote(
                    operation,
                    SwapUseId::first(operation),
                    &approval,
                    QuoteResult {
                        orderbook: None,
                        bridge_clients: None,
                        bridge: None,
                        outcome: Ok(QuoteOutcome::PriceBlocked(PriceBlock::Deviates)),
                    },
                    window,
                    cx,
                );
                assert_eq!(
                    swaps.form.as_ref().and_then(SwapForm::operation),
                    Some(operation)
                );
                assert!(swaps.reapproval.is_some());
            });
            window.draw(cx).clear(cx);
        });

        // Authorization starts work from the review form. A slow submission must
        // immediately hand over to progress, where failures remain retryable.
        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                assert!(swaps.form.is_some());
                swaps.start_job(
                    operation,
                    SwapJobKind::Order,
                    async move {
                        receive.await.unwrap();
                        Err::<(), _>(eyre::eyre!("order unavailable"))
                    },
                    |_, (), _, _| unreachable!(),
                    window,
                    cx,
                );
                assert!(swaps.form.is_none(), "the disabled form is replaced");
                assert!(swaps.detail_is_active(operation, window, cx));
                assert!(swaps.busy());
            });
            window.draw(cx).clear(cx);
        });
        send.send(()).unwrap();
        runtime.block_on(tokio::task::yield_now());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let swaps = swaps.read(cx);
            assert!(!swaps.busy());
            assert!(swaps.detail_is_active(operation, window, cx));
            assert_eq!(
                swaps.tracking.get(&operation).unwrap().error.as_deref(),
                Some("order unavailable")
            );
        });
    });
}

/// My orders focuses its list. With no row to show, the empty state holds that focus, so
/// Escape still reaches the dialog.
#[gpui::test]
fn my_orders_without_rows_keeps_focus_in_the_dialog(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let row: &'static str = format!("swap-order-row-{}", operation.opaque_id()).leak();
        let open_orders = |filter: Option<model::SwapOrderGroup>,
                           cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.orders_filter = filter;
                    swaps.show_view(dialog::SwapDialogView::Orders, window, cx);
                });
                window.draw(cx).clear(cx);
            });
        };
        let escape_closes = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| assert!(swaps.read(cx).swap_dialog_active(window, cx)));
            cx.simulate_keystrokes("escape");
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(!window.has_active_dialog(cx));
                assert!(swaps.read(cx).dialog.is_none());
            });
        };
        // The only swap is open, so Ended lists nothing.
        open_orders(Some(model::SwapOrderGroup::Ended), cx);
        assert!(cx.debug_bounds(row).is_none());
        escape_closes(cx);
        // A reload can empty the group on screen while its list has focus.
        open_orders(Some(model::SwapOrderGroup::Open), cx);
        assert!(cx.debug_bounds(row).is_some());
        executors.retire(operation).unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                cx.notify();
            });
            window.refresh();
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds(row).is_none());
        escape_closes(cx);
    });
}

/// A swap whose unshield ran and whose order expired unfilled. Its sell token waits in the
/// stealth account, so Stealth accounts offers recovery without a balance check.
fn stranded_swap(executors: &ExecutorStore, operation: ExecutorOperationId) {
    use wallet_ops::vault::{SwapDelivery, SwapObservation, SwapOrderObservations};
    let (uid, observed) = placed_swap(executors, operation, SwapDelivery::Reshield);
    let seen = SwapObservation {
        block: observed.block(),
        transaction_hash: None,
    };
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
}

/// An order of Address 1 for Address 2 delivered as `delivery`, placed from a set-up stealth
/// account at `Address::repeat_byte(3)`, with nothing observed yet. A Bridge order deposits its
/// 99 bought for [`BRIDGE_MINIMUM`] on the destination network, and a NEAR Intents one pays
/// [`NEAR_DEPOSIT_ADDRESS`]. Returns its UID and the account's observation.
fn placed_swap(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delivery: wallet_ops::vault::SwapDelivery,
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    placed_swap_with(executors, operation, delivery, |_| {})
}

struct PlacedPrivateBridge {
    operation: ExecutorOperationId,
    destination_operation: ExecutorOperationId,
    delivery: BridgeDelivery,
    uid: broadcaster_core::contracts::cow::OrderUid,
    origin_observed: wallet_ops::vault::ExecutorNonceObservation,
    destination_observed: wallet_ops::vault::ExecutorNonceObservation,
}

/// Linked Ethereum and Polygon accounts with a private Bridge order and its destination
/// shield signed. Neither the order's delivery nor the shield's execution is observed yet.
fn placed_private_bridge(
    origin: &ExecutorStore,
    destination: &ExecutorStore,
    origin_delegate: Address,
    destination_delegate: Address,
    destination_token: Address,
) -> PlacedPrivateBridge {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, SwapDestinationRecord,
    };

    let operation = ExecutorOperationId::random().unwrap();
    let destination_operation = ExecutorOperationId::random().unwrap();
    let receiver = Address::repeat_byte(0x51);
    origin
        .reserve_with_swap_approval(
            operation,
            origin_delegate,
            Some("Private swap"),
            &[],
            None,
            Some(destination_operation),
        )
        .unwrap();
    destination
        .reserve_swap_destination(
            destination_operation,
            destination_delegate,
            SwapDestinationRecord {
                origin_chain: 1,
                origin_operation: operation,
                destination_token,
                outcome: None,
            },
        )
        .unwrap();
    destination
        .bind_address(destination_operation, receiver)
        .unwrap();
    // The destination account's setup won its first nonce, and its shield is signed
    // at the next one.
    let setup_hash = B256::repeat_byte(69);
    record_setup(
        destination,
        destination_operation,
        destination_delegate,
        setup_hash,
        Vec::new(),
    );
    let destination_observed =
        ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
    confirm_setup(destination, destination_operation, destination_observed);
    destination
        .record_issued(
            destination_operation,
            IssuedExecutorPayload::new(
                U256::ONE,
                destination_delegate,
                B256::repeat_byte(70),
                ExecutorPayloadPurpose::SwapDestinationShield,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"shield"),
                    destination_observed,
                    Vec::new(),
                ),
            ),
        )
        .unwrap();
    let delivery = BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver,
        destination_token,
        surplus: BridgeSurplus::Reshield,
        private: Some(BridgePrivateDelivery {
            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
        }),
    };
    let (uid, origin_observed) = placed_swap(origin, operation, SwapDelivery::Bridge(delivery));
    PlacedPrivateBridge {
        operation,
        destination_operation,
        delivery,
        uid,
        origin_observed,
        destination_observed,
    }
}

/// [`placed_swap`] with `edit` applied to the order's approved bounds.
fn placed_swap_with(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delivery: wallet_ops::vault::SwapDelivery,
    edit: impl FnOnce(&mut wallet_ops::vault::SwapApprovedBounds),
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        AcrossOrderTerms, BridgeOrderTerms, BridgeProvider, ExecutorInputIdentity,
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, NearIntentsOrderTerms, SwapAttempt, SwapDelivery, SwapProof,
        SwapRecipient, SwapTerms,
    };
    let setup = pending_setup(executors, operation);
    let setup_hash = setup.issued()[0].hash();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
    executors.record_account_read(operation, observed).unwrap();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16198, "commitment": "0x2"
    }))
    .unwrap();
    let uid = OrderUid::new(B256::repeat_byte(0x11), Address::repeat_byte(3), 20);
    let hook = |nonce, hash, purpose, inputs| {
        IssuedExecutorPayload::new(
            U256::from(nonce),
            setup.delegate(),
            B256::repeat_byte(hash),
            purpose,
            ExecutorPayloadContext::new(Bytes::from_static(b"hook"), observed, inputs),
        )
    };
    let mut bounds = test_approval().bounds;
    let post_hook = hook(2_u64, 8, ExecutorPayloadPurpose::SwapPostHook, Vec::new());
    // A Private delivery shields the output with a post-hook, and an Across one deposits it.
    let (post_hook, bridge) = match delivery {
        SwapDelivery::Reshield => (Some(post_hook), None),
        SwapDelivery::External { .. } => {
            bounds.post_hook_gas_limit = None;
            (None, None)
        }
        SwapDelivery::Bridge(delivery) => {
            bounds.destination_minimum = Some(BRIDGE_MINIMUM);
            match delivery.provider {
                BridgeProvider::Across => (
                    Some(post_hook),
                    Some(BridgeOrderTerms::Across(AcrossOrderTerms {
                        spoke_pool: Address::repeat_byte(0x55),
                        input_token: Address::repeat_byte(2),
                        output_token: delivery.destination_token,
                        input_amount: bounds.buy_amount,
                        output_amount: BRIDGE_MINIMUM,
                        quote_timestamp: 1_790_000_000,
                        fill_deadline: 1_790_007_200,
                        exclusive_relayer: Address::ZERO,
                        exclusivity_parameter: 0,
                        recipient: delivery.is_private().then_some(Address::repeat_byte(0x56)),
                        message_hash: delivery.is_private().then_some(B256::repeat_byte(0x57)),
                    })),
                ),
                BridgeProvider::NearIntents => {
                    bounds.post_hook_gas_limit = None;
                    (
                        None,
                        Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
                            deposit_address: NEAR_DEPOSIT_ADDRESS,
                            min_amount_out: BRIDGE_MINIMUM,
                            amount_out: BRIDGE_MINIMUM,
                            deadline: "2026-09-30T01:00:00.000Z".into(),
                            signed_quote: "{}".into(),
                        })),
                    )
                }
            }
        }
    };
    edit(&mut bounds);
    executors
        .record_swap_attempt(
            operation,
            SwapAttempt {
                use_id: wallet_ops::vault::SwapUseId::first(operation),
                terms: SwapTerms::new(
                    Address::repeat_byte(1),
                    Address::repeat_byte(2),
                    SwapRecipient::new(U256::ONE, [7; 32]),
                    setup_hash,
                ),
                proof: SwapProof::new(B256::repeat_byte(6), vec![input.clone()]),
                uid,
                submission: None,
                delivery,
                bounds,
                invalidates: None,
                pre_hook: hook(1_u64, 7, ExecutorPayloadPurpose::SwapPreHook, vec![input]),
                post_hook,
                bridge,
            },
        )
        .unwrap();
    (uid, observed)
}

#[gpui::test]
fn reused_account_progress_keeps_the_new_swap_separate_from_its_history(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorInputIdentity, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload, SwapAccountChoice, SwapApprovalTokens,
        SwapAttempt, SwapDelivery, SwapObservation, SwapOrderObservations, SwapPairClaim,
        SwapProof, SwapTerms, SwapTradeAmounts, SwapUseId,
    };

    with_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        stranded_swap(executors, operation);
        let mut record = executors.records().unwrap().pop().unwrap();
        let previous = record.swap().unwrap().orders().last().unwrap().uid();
        // The account's first swap, and the use a draft claims the account for next.
        let original = SwapIdentity {
            operation,
            swap_use: SwapUseId::first(operation),
        };
        let second = SwapUseId::random().unwrap();
        let pending = PendingSwapOrder {
            previous_order: None,
            sell: Address::repeat_byte(2),
            buy: Address::repeat_byte(1),
            delivery: SwapDelivery::Reshield,
            amount: U256::from(50),
            private_minimum: U256::from(45),
            slippage_bps: 100,
            gas_share_bps: wallet_ops::cow::GAS_SHARE_TIGHT_BPS,
            valid_for: Duration::from_mins(30),
            reuse_account: true,
            swap_use: second,
            started_at: 1,
        };
        // The original swap's unshielded funds wait in the account. A draft that sells
        // another token doesn't change what its recovery starts with.
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().pending_order = Some(pending);
                let record = swaps.record(operation).unwrap();
                assert!(swaps.stage(record).needs_recovery());
                assert_eq!(swaps.recovery_token(record), Some(Address::repeat_byte(1)));
                swaps.tracking.entry(operation).or_default().pending_order = None;
            });
        });
        let seen = SwapObservation {
            block: record.nonce_observation().unwrap().block(),
            transaction_hash: Some(B256::repeat_byte(40)),
        };
        record = executors
            .record_swap_observations(
                operation,
                previous,
                SwapOrderObservations {
                    pre_hook_executed: Some(seen),
                    traded: Some(seen),
                    delivered: Some(seen),
                    shielded: Some(
                        serde_json::from_value(serde_json::json!({
                            "observation": seen, "private_amount": "0x62", "fee": "0x1"
                        }))
                        .unwrap(),
                    ),
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
        executors.set_hidden(operation, true).unwrap();
        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        let completed = cx.update(|window, cx| {
            let completed = swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let labels = swaps.labels(swaps.record(operation).unwrap(), cx);
                let completed = (labels.pair, labels.received);
                assert!(completed.1.is_some());
                swaps.tracking.entry(operation).or_default().pending_order = Some(pending);
                swaps.start_job(
                    operation,
                    SwapJobKind::Order,
                    async move {
                        receive.await.unwrap();
                        Err::<(), _>(eyre::eyre!("proof preparation failed"))
                    },
                    |_, (), _, _| unreachable!(),
                    window,
                    cx,
                );
                let record = swaps.record(operation).unwrap();
                assert_eq!(swaps.stage(record), SwapStage::Order(SwapOrderState::Done));
                assert_eq!(swaps.progress_stage(record), SwapStage::Ready);
                assert_eq!(
                    swaps.progress_title(operation, cx),
                    format!(
                        "Swap {} for {}",
                        swaps.token_amount(pending.sell, pending.amount, cx),
                        swaps.token_symbol(pending.buy, cx)
                    ),
                );
                assert!(
                    swaps.has_shown_swaps(),
                    "a previously hidden account shows new work"
                );
                // The completed swap keeps its own amounts under its own entry.
                let (record, _, order) = swaps.past_swap(original, 0).unwrap();
                assert_eq!(order.uid(), previous);
                let past = swaps.past_labels(record, order, cx);
                assert_eq!((past.pair, past.received), completed);
                assert_ne!(swaps.labels(record, cx).pair, completed.0);
                assert_eq!(pending.started(record), pending.started_at);
                completed
            });
            window.draw(cx).clear(cx);
            completed
        });
        assert!(cx.debug_bounds("swap-outcome").is_none());

        // The previous result remains accessible, but closing and reopening the current
        // swap from Private must keep showing the new submission.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.show_view(dialog::SwapDialogView::PastDetail(original, 0), window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_some());
        cx.update(|window, cx| {
            window.close_all_dialogs(cx);
            swaps.update(cx, |swaps, cx| swaps.open_details(window, cx));
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_none());

        // A failure before persistence must retry the new terms, not the completed swap.
        send.send(()).unwrap();
        runtime.block_on(tokio::task::yield_now());
        cx.run_until_parked();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                assert!(!swaps.busy());
                assert_eq!(
                    swaps.progress_stage(swaps.record(operation).unwrap()),
                    SwapStage::Ready
                );
                swaps.open_existing_form(operation, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!((form.sell, form.buy), (pending.sell, Some(pending.buy)));
                assert_eq!(swaps.form_amount(form, cx).unwrap(), pending.amount);
                assert_eq!(form.slippage_bps, pending.slippage_bps);
                // The retry restores the share and the validity the order was approved with.
                assert_eq!(
                    (form.gas_share_bps, form.valid_for),
                    (pending.gas_share_bps, pending.valid_for)
                );
                assert!(form.reuse_account);
                assert_eq!(form.reuse_use, Some(second));
                swaps.start_job(
                    operation,
                    SwapJobKind::Order,
                    std::future::pending::<eyre::Result<()>>(),
                    |_, (), _, _| unreachable!(),
                    window,
                    cx,
                );
            });
        });

        // The submission claims the account for the draft's use before it signs. The swap then
        // starts when that use did, and still shows the draft's terms, not the earlier order's.
        let mut bounds = test_approval().bounds;
        bounds.sell_amount = pending.amount;
        bounds.private_minimum = pending.private_minimum;
        let claimed = executors
            .claim_swap_pair(SwapPairClaim {
                id: second,
                source: SwapAccountChoice::Existing(operation),
                delegate: record.delegate(),
                purpose_summary: None,
                assets: Vec::new(),
                approval: SwapApproval {
                    bounds: bounds.clone(),
                    tokens: Some(SwapApprovalTokens {
                        sell: pending.sell,
                        buy: pending.buy,
                    }),
                    ..test_approval()
                },
                destination: None,
            })
            .unwrap()
            .source;
        let claimed_at = claimed.swap_use(second).unwrap().started_at().unwrap();
        assert_ne!(claimed_at, pending.started_at);
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert_eq!(swaps.pending_order(record).unwrap().swap_use, second);
                assert_eq!(pending.started(record), claimed_at);
                assert_eq!(
                    swaps.progress_title(operation, cx),
                    format!(
                        "Swap {} for {}",
                        swaps.token_amount(pending.sell, pending.amount, cx),
                        swaps.token_symbol(pending.buy, cx)
                    ),
                );
                assert_eq!(swaps.past_swap(original, 0).unwrap().2.uid(), previous);
            });
        });

        // Persist the second order while submission is still in flight. Its UID and durable
        // progress immediately replace the preparation view; the old result stays separate.
        let observed = ExecutorNonceObservation::new(
            BlockNumHash::new(40, B256::repeat_byte(40)),
            U256::from(3),
        );
        let setup = &record.issued()[0];
        executors.record_account_read(operation, observed).unwrap();
        let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
            "tree": 4, "position": 16199, "commitment": "0x3"
        }))
        .unwrap();
        let hook = |nonce, hash, purpose, inputs| {
            IssuedExecutorPayload::new(
                U256::from(nonce),
                record.delegate(),
                B256::repeat_byte(hash),
                purpose,
                ExecutorPayloadContext::new(Bytes::from_static(b"new hook"), observed, inputs),
            )
        };
        let uid = OrderUid::new(B256::repeat_byte(0x12), Address::repeat_byte(3), u32::MAX);
        let terms = record.swap().unwrap().terms();
        executors
            .record_swap_attempt(
                operation,
                SwapAttempt {
                    use_id: second,
                    terms: SwapTerms::new(
                        pending.sell,
                        pending.buy,
                        terms.recipient(),
                        setup.hash(),
                    ),
                    proof: SwapProof::new(B256::repeat_byte(9), vec![input.clone()]),
                    uid,
                    submission: None,
                    delivery: SwapDelivery::Reshield,
                    bounds,
                    invalidates: None,
                    pre_hook: hook(3_u64, 9, ExecutorPayloadPurpose::SwapPreHook, vec![input]),
                    post_hook: Some(hook(
                        4_u64,
                        10,
                        ExecutorPayloadPurpose::SwapPostHook,
                        Vec::new(),
                    )),
                    bridge: None,
                },
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert!(swaps.busy());
                assert!(swaps.pending_order(record).is_none());
                assert_eq!(swaps.progress_stage(record), SwapStage::SubmissionPending);
                assert_eq!(record.swap().unwrap().orders().last().unwrap().uid(), uid);
                // The new order is the second use's alone. The first swap keeps its order,
                // amounts and outcome, and the new swap keeps the start of its use.
                let (record, _, order) = swaps.past_swap(original, 0).unwrap();
                assert_eq!(order.uid(), previous);
                let past = swaps.past_labels(record, order, cx);
                assert_eq!((past.pair, past.received), completed);
                let (_, latest) = swaps.record_history(record);
                assert_eq!(latest.as_ref().map(|swap| swap.swap_use), Some(second));
                assert_eq!(
                    swaps.swap_started(record, latest.as_ref(), cx),
                    Some(("Started", claimed_at))
                );
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_none());
    });
}

/// Open the swap's detail and press Recover…, which reveals the account in Stealth accounts
/// behind the dialogs and then asks recovery to open.
fn press_swap_recover(
    swaps: &Entity<PrivateSwapsView>,
    operation: ExecutorOperationId,
    stage: SwapStage,
    cx: &mut gpui::VisualTestContext,
) {
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            swaps.reload_records();
            assert_eq!(swaps.stage(swaps.record(operation).unwrap()), stage);
            swaps.show_detail(operation, window, cx);
        });
        window.draw(cx).clear(cx);
    });
    let recover = cx.debug_bounds("swap-progress-recover").unwrap();
    cx.simulate_click(recover.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

/// Closing recovery returns focus to the swap's detail under it, not to the account revealed
/// behind both dialogs.
#[gpui::test]
fn closing_swap_recovery_returns_focus_to_the_swap_detail(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        stranded_swap(executors, operation);
        press_swap_recover(
            swaps,
            operation,
            SwapStage::Order(wallet_ops::SwapOrderState::PreHookOnly { expired: true }),
            cx,
        );
        assert!(cx.debug_bounds("stealth-recovery-form").is_some());
        cx.update(|window, cx| {
            let swaps = swaps.read(cx);
            assert!(swaps.dialog.is_some());
            assert!(!swaps.swap_dialog_active(window, cx));
        });
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(swaps.read(cx).detail_is_active(operation, window, cx));
        });
        assert!(cx.debug_bounds("stealth-recovery-form").is_none());
        // The dialog layer restores the focus recorded when recovery opened, after its close
        // animation.
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.has_active_dialog(cx));
            assert!(swaps.read(cx).detail_is_active(operation, window, cx));
        });
    });
}

/// Recovery can decline after the reveal, here for a retired setup whose balances were never
/// checked. The swap's detail keeps focus.
#[gpui::test]
fn declined_swap_recovery_leaves_focus_in_the_swap_detail(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        executors.retire(operation).unwrap();
        press_swap_recover(swaps, operation, SwapStage::SetupRetired, cx);
        assert!(cx.debug_bounds("stealth-recovery-form").is_none());
        cx.update(|window, cx| {
            assert!(swaps.read(cx).detail_is_active(operation, window, cx));
        });
    });
}

#[gpui::test]
fn expired_order_can_be_checked_and_hidden_without_releasing_inputs(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let (uid, _) = placed_swap(executors, operation, SwapDelivery::Reshield);
        executors
            .record_swap_submission(
                operation,
                uid,
                wallet_ops::vault::SwapSubmissionStatus::Accepted,
            )
            .unwrap();
        let reserved = executors.records().unwrap()[0].reserved_inputs();
        assert!(!reserved.is_empty());
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let check = cx.debug_bounds("swap-progress-check").unwrap();
        cx.simulate_click(check.center(), gpui::Modifiers::none());
        cx.update(|_, cx| {
            assert!(
                swaps
                    .read(cx)
                    .tracking
                    .get(&operation)
                    .is_some_and(|tracking| tracking.error.is_some()),
                "a check without a synced head must explain why it cannot run"
            );
        });
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture")
                };
                sync_tip.head_block = Some(100);
            });
            window.draw(cx).clear(cx);
        });
        let check = cx.debug_bounds("swap-progress-check").unwrap();
        cx.simulate_click(check.center(), gpui::Modifiers::none());
        cx.update(|_, cx| {
            let swaps = swaps.read(cx);
            assert_eq!(swaps.job.as_ref().unwrap().kind, SwapJobKind::Check);
            assert!(swaps.tracking[&operation].error.is_none());
        });
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| swaps.job.is_none())
        });
        cx.update(|window, cx| {
            assert!(swaps.read(cx).tracking[&operation].error.is_some());
            window.draw(cx).clear(cx);
        });
        let remove = cx.debug_bounds("swap-progress-remove").unwrap();
        cx.simulate_click(remove.center(), gpui::Modifiers::none());
        cx.update(|_, cx| {
            let swaps = swaps.read(cx);
            assert!(!swaps.has_shown_swaps());
            assert!(swaps.next_order_hints(cx).is_some(), "tracking continues");
            let record = swaps.record(operation).unwrap();
            assert_eq!(swaps.stage(record), SwapStage::Order(SwapOrderState::Open));
            assert!(matches!(
                model::swap_actions(swaps.stage(record), true).retry,
                Some(Err(_))
            ));
            assert_eq!(
                model::swap_order_group(swaps.stage(record), false, record.is_hidden()),
                model::SwapOrderGroup::Open,
                "hiding the card must not claim the swap has ended"
            );
        });
        let saved = executors.records().unwrap().pop().unwrap();
        assert!(saved.is_hidden());
        assert_eq!(saved.reserved_inputs(), reserved);
        assert_eq!(saved.swap().unwrap().orders()[0].uid(), uid);
        assert_eq!(
            saved.swap().unwrap().orders()[0].observations(),
            wallet_ops::vault::SwapOrderObservations::default()
        );
    });
}

#[gpui::test]
fn routine_order_polling_waits_for_a_settlement_hint_without_reconciling_history(
    cx: &mut TestAppContext,
) {
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        stranded_swap(executors, operation);
        let record = executors.records().unwrap().pop().unwrap();
        let uid = record.swap().unwrap().orders()[0].uid();
        executors
            .record_swap_observations(
                operation,
                uid,
                wallet_ops::vault::SwapOrderObservations::default(),
            )
            .unwrap();
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture")
                };
                sync_tip.head_block = Some(400);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let confirmed = swaps.confirmed_block(cx).unwrap();
                assert!(
                    swaps.next_observations(cx).is_none(),
                    "no account RPC while waiting for CoW"
                );
                assert!(
                    swaps.next_order_hints(cx).is_some(),
                    "an order filled while offline must still be located after expiry"
                );
                let result = |block| HintResult {
                    operation,
                    uid,
                    client: None,
                    fee: false,
                    report: Some((
                        CowOrderStatusReport {
                            status: CowOrderStatusHint::Fulfilled,
                        },
                        Some(block),
                    )),
                    bridges: Vec::new(),
                };
                swaps.apply_order_hints(vec![result(confirmed + 1)], cx);
                assert!(
                    swaps.next_observations(cx).is_none(),
                    "wait for safe depth locally"
                );
                swaps.apply_order_hints(vec![result(confirmed)], cx);
                let (_, _, pages) = swaps.next_observations(cx).unwrap();
                assert_eq!(pages.len(), 1);
                assert_eq!(pages[0].settlement, Some((uid, confirmed)));
                assert!(
                    pages[0]
                        .range
                        .as_ref()
                        .is_some_and(|range| range.end > confirmed),
                    "no immediate account-history catchup loop"
                );
                assert!(
                    swaps.next_order_hints(cx).is_some(),
                    "a hint remains refreshable until verified"
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("swap-progress-check").is_some(),
            "an expired unresolved order offers an explicit account check"
        );

        // Once verified complete, the order's open detail asks the orderbook nothing more.
        let seen = wallet_ops::vault::SwapObservation {
            block: record.nonce_observation().unwrap().block(),
            transaction_hash: Some(alloy::primitives::B256::repeat_byte(40)),
        };
        executors
            .record_swap_observations(
                operation,
                uid,
                wallet_ops::vault::SwapOrderObservations {
                    pre_hook_executed: Some(seen),
                    traded: Some(seen),
                    delivered: Some(seen),
                    shielded: Some(
                        serde_json::from_value(serde_json::json!({
                            "observation": seen, "private_amount": "0x62", "fee": "0x1"
                        }))
                        .unwrap(),
                    ),
                    ..Default::default()
                },
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().order_hint = None;
                let record = swaps.record(operation).unwrap();
                assert_eq!(swaps.stage(record), SwapStage::Order(SwapOrderState::Done));
                assert!(swaps.detail_is_active(operation, window, cx));
                assert!(
                    swaps.next_order_hints(cx).is_none(),
                    "a completed order's detail must not ask the orderbook about it"
                );
            });
        });
    });
}

/// A reusable set-up stealth account, whose address is `Address::repeat_byte(3)`.
fn reusable_account(executors: &ExecutorStore, operation: ExecutorOperationId) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorNonceObservation;
    pending_setup(executors, operation);
    confirm_setup(
        executors,
        operation,
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
    );
}

/// Record a confirmed read of the account's execution nonce past its setup's.
fn confirm_setup(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    observed: wallet_ops::vault::ExecutorNonceObservation,
) -> wallet_ops::vault::ExecutorRecord {
    executors.record_account_read(operation, observed).unwrap()
}

fn cold_wallet_entry(address: Address) -> wallet_ops::vault::PublicAddressBookEntry {
    wallet_ops::vault::PublicAddressBookEntry {
        entry_uuid: "cold-wallet".into(),
        label: "Cold wallet".into(),
        address,
        display_order: 0,
    }
}

fn set_receiver_text(
    swaps: &mut PrivateSwapsView,
    text: &str,
    window: &mut Window,
    cx: &mut Context<'_, PrivateSwapsView>,
) {
    let input = swaps.form.as_ref().unwrap().receiver_input.clone();
    input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    // Programmatic input changes don't emit InputEvent::Change.
    swaps.receiver_edited(window, cx);
}

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

/// The Buy list is ERC-20 only. A Public address receiving the chain's wrapped native token
/// can take the native asset instead, through Private Unshield's output switch, and the order
/// then buys the native marker.
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
                assert!(!offers_native(swaps, cx), "the Buy list stays ERC-20");
            });
        });
        assert_eq!(
            shown(cx),
            (true, false),
            "WETH to a Public address offers ETH"
        );

        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_native_output(true, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.buy, Some(weth));
                assert_eq!(form.order_buy(), Some(Address::ZERO));
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
                    swaps.swap_summary(&review, None, None, None, cx),
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
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                root.public_address_book = vec![cold_wallet_entry(receiver)];
            });
        });
        let delivery = SwapDelivery::External { receiver };
        let (uid, observed) = placed_swap(executors, operation, delivery);
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
                        swap_recovery_token(stage, delivery, sell, buy),
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
    with_swap_view_and_rpc(
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
    with_swap_view_and_rpc(
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
                root.chain_states
                    .insert(42161, ChainUtxoState::Loading { progress: None });
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
/// the reason the picker shows. A network with such an account and no setup funds can be
/// picked, and says that an existing account is needed.
#[test]
fn private_balance_needs_a_synced_and_funded_network() {
    let funded = PrivateNetworkFacts {
        rpc: true,
        swap_profile: true,
        sync: NetworkSync::Ready,
        funded: true,
        reusable: false,
    };
    let unavailable = NetworkAvailability::Unavailable;
    for (facts, availability) in [
        (funded, NetworkAvailability::Available),
        (
            PrivateNetworkFacts {
                funded: false,
                ..funded
            },
            unavailable(NetworkUnavailable::Unfunded),
        ),
        (
            PrivateNetworkFacts {
                funded: false,
                reusable: true,
                ..funded
            },
            NetworkAvailability::ReuseOnly,
        ),
        // A set-up account doesn't stand in for the sync, which reads it.
        (
            PrivateNetworkFacts {
                sync: NetworkSync::Loading(None),
                funded: false,
                reusable: true,
                ..funded
            },
            NetworkAvailability::Syncing(None),
        ),
        // Funds can't be read before the sync is ready, so a loading chain says so first.
        (
            PrivateNetworkFacts {
                sync: NetworkSync::Loading(Some(62)),
                funded: false,
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
    assert!(reuse.is_available(), "its routes stay selectable");
    assert!(!unavailable(NetworkUnavailable::Unfunded).is_available());
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
                .map(|network| (network.chain_id, network.availability))
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
                enable_stub_chain(root, &stubs, 8453);
                // A Railgun chain with a higher id than Base, which the selector lists first.
                enable_stub_chain(root, &stubs, 42161);
                // A chain the user added has no Railgun deployment and no 1Click name, and the
                // stub Across has no route to it.
                let mut added = root.effective_chain_configs.get(8453).unwrap().clone();
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
                        .map(|network| network.chain_id)
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    chains(swaps, ReceiveTo::PrivateBalance, cx),
                    [1, 42161],
                    "only the chains with a private balance, and disabled Optimism isn't listed"
                );
                assert_eq!(
                    chains(swaps, ReceiveTo::PublicAddress, cx),
                    [1, 42161, 8453, ADDED],
                    "the same order, with the Public-address-only chains after"
                );
                let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
                assert_eq!(content.receive_to, ReceiveTo::PrivateBalance);
                assert_eq!(
                    content.network_counts,
                    (2, 4),
                    "Base and the added chain take a Public address only"
                );
            });
            let states = &root.read(cx).chain_states;
            assert!(
                !states.contains_key(&8453) && !states.contains_key(&ADDED),
                "no session starts on a chain without Railgun"
            );
            swaps.update(cx, |swaps, cx| {
                swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                set_receiver_text(swaps, &Address::repeat_byte(4).to_string(), window, cx);
                assert_eq!(
                    listed(swaps, ReceiveTo::PublicAddress, cx),
                    [
                        (42161, NetworkAvailability::Available),
                        (8453, NetworkAvailability::Available),
                        (ADDED, NetworkAvailability::Available),
                    ],
                    "a chain can be picked before its routes are fetched"
                );
                let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
                assert_eq!(
                    content.network_counts,
                    (2, 4),
                    "the counts don't depend on the delivery kind"
                );

                // The network search filters what the picker lists, by name or by the start
                // of the chain id, and leaves the form's own list alone.
                let networks = swaps.form.as_ref().unwrap().picker.networks.clone();
                let mut matches = |query: &'static str, cx: &mut Context<'_, PrivateSwapsView>| {
                    networks.update(cx, |list, cx| {
                        list.set_query(query, window, cx);
                        list.delegate().listed().collect::<Vec<_>>()
                    })
                };
                assert_eq!(matches(" ARBITRUM ", cx), [42161]);
                assert_eq!(matches("7777", cx), [ADDED]);
                assert_eq!(listed(swaps, ReceiveTo::PublicAddress, cx).len(), 3);
                assert_eq!(matches("", cx), [1, 42161, 8453, ADDED]);
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
                    (8453, NetworkAvailability::Available),
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
            relay_adapt: Address::ZERO,
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
                    vec![candidate.clone()]
                )),
                expected,
                "a compatible fee token alone is insufficient",
            );
        }
        let invalid = PublicBroadcasterCandidate {
            relay_adapt_7702: None,
            ..candidate
        };
        assert_eq!(
            runtime.block_on(buy_picker::estimate_network_funding(
                &session,
                vec![invalid]
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
            ] {
                match refresh {
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
                    });
                });
            }
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

/// A reusable set-up stealth account in the fixture wallet's records on Polygon, whose address
/// is `Address::repeat_byte(3)`, with the store that holds it.
fn reusable_polygon_account(
    root: &Entity<WalletRoot>,
    cx: &gpui::VisualTestContext,
) -> (ExecutorStore, ExecutorOperationId) {
    let (vault, view, delegate) = root.read_with(cx, |root, _| {
        (
            root.vault_store.clone().unwrap(),
            root.view_session.clone().unwrap(),
            root.effective_chain_configs
                .get(137)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
                .delegate(),
        )
    });
    let executors = ExecutorStore::new(vault.db(), view, 137).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    executors
        .reserve(
            operation,
            delegate,
            Some("Private swap"),
            &[wallet_ops::ExecutorAsset::Erc20(STUB_POLYGON_USDT)],
        )
        .unwrap();
    reusable_account(&executors, operation);
    (executors, operation)
}

/// Choose `account` in a stealth account select, as a click on its row does.
fn confirm_account(
    select: &Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>,
    account: Option<ExecutorOperationId>,
    window: &mut Window,
    cx: &mut App,
) {
    select.update(cx, |select, cx| {
        select.set_selected_value(&account, window, cx);
        cx.emit(SelectEvent::<SearchableVec<SwapAccountSelectItem>>::Confirm(Some(account)));
    });
}

/// Private balance on another network chooses a stealth account on each network by itself.
/// Polygon holds no private funds and no broadcaster offers, only a set-up account: its routes
/// stay selectable, a new account there leaves Review unavailable with the setup-funding
/// reason, and nothing selects the existing one. Choosing it needs no setup there. A refused
/// choice stays with its reason, a token change keeps the choice and checks it again, a
/// network change returns the destination to a new account and keeps the source, and a new
/// session's form has neither choice. Any other delivery has one selector.
#[gpui::test]
fn private_bridge_accounts_are_chosen_independently(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, store, cx| {
            reusable_account(executors, operation);
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            let session = start_polygon_session(root, &stubs, runtime, store, cx);
            let (polygon, destination) = reusable_polygon_account(root, cx);
            let chosen = |swaps: &PrivateSwapsView| {
                let form = swaps.form.as_ref().unwrap();
                (
                    form.operation,
                    form.destination_choice().map(|account| account.operation),
                )
            };
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    assert_eq!(
                        swaps.network_availability(ReceiveTo::PrivateBalance, 137, cx),
                        Some(NetworkAvailability::ReuseOnly)
                    );
                    assert_eq!(chosen(swaps), (None, None), "nothing selects the account");
                    let form = swaps.form.as_ref().unwrap();
                    assert!(
                        matches!(
                            &form.delivery,
                            Err(DeliveryProblem::Network { syncing: false, .. })
                        ),
                        "a new account there needs setup funds: {:?}",
                        form.delivery
                    );
                    assert!(form.quote_delivery().is_none(), "Review stays unavailable");
                    // The picker keeps Polygon and its routes.
                    swaps.open_buy_picker(window, cx);
                    assert_eq!(picker_polygon(swaps, cx), NetworkAvailability::ReuseOnly);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(form.picker.network, 137);
                    let content = swaps.buy_picker_content(form, cx);
                    assert!(matches!(
                        &content.tokens,
                        buy_picker::BuyPickerTokens::Listed(rows)
                            if rows.iter().any(|row| row.item.asset.token == STUB_POLYGON_USDT)
                    ));
                    swaps.close_buy_picker(cx);
                    window.close_dialog(cx);
                });
            });
            // A funded destination still needs no setup when its existing account is used.
            // Fail the estimator's RPC after sync, without changing its notes or offers.
            session
                .executor_owner()
                .unwrap()
                .plan_swaps_from_note_for_tests(STUB_POLYGON_USDT, U256::from(100_000_000));
            let mut offer = crate::root::tests::fee_row(137, STUB_POLYGON_USDT, "reuse-offer");
            offer.relay_adapt_7702 = root.read_with(cx, |root, _| {
                root.effective_chain_configs
                    .get(137)
                    .unwrap()
                    .accepted_executor_profile()
                    .map(wallet_ops::settings::ExecutorProfile::delegate)
            });
            root.update(cx, |root, _| {
                let ChainUtxoState::Ready { snapshot, .. } =
                    root.chain_states.get_mut(&137).unwrap()
                else {
                    panic!("ready destination session")
                };
                *snapshot = Arc::new(wallet_ops::ListUtxosOutput {
                    chain_id: 137,
                    cache_key: "reuse-funding-test".into(),
                    utxo_count: 1,
                    unspent_count: 1,
                    spent_count: 0,
                    local_pending_spent_count: 0,
                    utxos: vec![crate::root::tests::unshield_utxo_output(
                        STUB_POLYGON_USDT,
                        100_000_000,
                        0,
                        1,
                    )],
                    totals: vec![wallet_ops::TokenTotal {
                        token: STUB_POLYGON_USDT.to_string(),
                        total: "100000000".into(),
                        poi_verified_total: "100000000".into(),
                    }],
                });
                root.monitor_state.write().upsert_fee(offer);
            });
            stubs.set_failing(&["/rpc"]);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.refresh_destination_network(window, cx);
                    assert_eq!(
                        swaps.network_setup_availability(137, cx),
                        NetworkAvailability::CheckingFee
                    );
                    swaps.open_buy_picker(window, cx);
                    assert_eq!(picker_polygon(swaps, cx), NetworkAvailability::ReuseOnly);
                    swaps.show_buy_picker_network(1, window, cx);
                    swaps.show_buy_picker_network(137, window, cx);
                    assert_eq!(swaps.form.as_ref().unwrap().picker.network, 137);
                    swaps.close_buy_picker(cx);
                    window.close_dialog(cx);
                    assert!(swaps.form.as_ref().unwrap().quote_delivery().is_none());
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
                    assert_eq!(
                        swaps.network_setup_availability(137, cx),
                        NetworkAvailability::Unavailable(NetworkUnavailable::SetupFee),
                        "the actual estimator reports the RPC failure"
                    );
                    swaps.refresh_destination_network(window, cx);
                    assert_eq!(chosen(swaps), (None, None));
                    assert!(swaps.form.as_ref().unwrap().quote_delivery().is_none());
                    swaps.open_buy_picker(window, cx);
                    assert_eq!(picker_polygon(swaps, cx), NetworkAvailability::ReuseOnly);
                    swaps.show_buy_picker_network(1, window, cx);
                    swaps.show_buy_picker_network(137, window, cx);
                    assert_eq!(swaps.form.as_ref().unwrap().picker.network, 137);
                    swaps.close_buy_picker(cx);
                    window.close_dialog(cx);
                    let select = swaps
                        .form
                        .as_ref()
                        .unwrap()
                        .destination_select
                        .clone()
                        .unwrap();
                    confirm_account(&select, Some(destination), window, cx);
                });
            });
            // The Select event applies after the view's update releases its borrow.
            swaps.read_with(cx, |swaps, _| {
                assert_eq!(chosen(swaps), (None, Some(destination)));
                assert!(swaps.form.as_ref().unwrap().quote_delivery().is_some());
            });
            // Later quote and signing checks still need a working RPC.
            stubs.set_failing(&[]);
            // The existing account is the delivery's receiver and takes no setup: no fee is
            // estimated on Polygon and no broadcaster is chosen there.
            let review = ready_review(swaps, runtime, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    assert_eq!(chosen(swaps), (None, Some(destination)));
                    let form = swaps.form.as_mut().unwrap();
                    assert!(matches!(
                        &form.delivery,
                        Ok(SwapDelivery::Bridge(delivery))
                            if delivery.receiver == Address::repeat_byte(3)
                    ));
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    assert_eq!(form.review_problem(&review), None);
                    assert!(!form.destination_route.is_used());
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(swaps.setup_chain(form, SetupSide::Origin), Some(1));
                    assert_eq!(swaps.setup_chain(form, SetupSide::Destination), None);

                    // The swap's own account is chosen apart from the destination.
                    swaps.select_form_account(Some(operation), window, cx);
                    assert_eq!(chosen(swaps), (Some(operation), Some(destination)));
                    let form = swaps.form.as_ref().unwrap();
                    assert!(!swaps.reviews_setup(form), "neither account needs setup");
                    swaps.select_form_account(None, window, cx);
                    assert_eq!(chosen(swaps), (None, Some(destination)));
                });
                window.draw(cx).clear(cx);
            });
            assert!(cx.debug_bounds("swap-account-source").is_some());
            assert!(cx.debug_bounds("swap-account-destination").is_some());
            assert!(
                cx.debug_bounds("swap-account-select").is_none(),
                "a private Bridge swap has two selectors"
            );
            assert!(cx.debug_bounds("swap-destination-reuse").is_some());
            assert!(cx.debug_bounds("swap-source-reuse").is_none());

            // Another token on Polygon keeps the choice, clears the acknowledgements and
            // checks the delivery to the account again.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_buy_picker(window, cx);
                    swaps.pick_buy_token(STUB_POLYGON_USDC, window, cx);
                    assert_eq!(chosen(swaps), (None, Some(destination)));
                    let form = swaps.form.as_ref().unwrap();
                    assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                    swaps.open_buy_picker(window, cx);
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    assert_eq!(chosen(swaps), (None, Some(destination)));
                    assert!(matches!(
                        &swaps.form.as_ref().unwrap().delivery,
                        Ok(SwapDelivery::Bridge(delivery))
                            if delivery.receiver == Address::repeat_byte(3)
                    ));
                });
            });
            // Once its records refuse the account, the choice stays and says why.
            polygon.retire(destination).unwrap();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.refresh_destination_network(window, cx);
                    assert_eq!(chosen(swaps), (None, Some(destination)), "the choice stays");
                    let form = swaps.form.as_ref().unwrap();
                    assert!(
                        matches!(&form.delivery, Err(DeliveryProblem::Account(_))),
                        "{:?}",
                        form.delivery
                    );
                    assert!(form.quote_delivery().is_none(), "Review stays unavailable");
                });
                window.draw(cx).clear(cx);
            });
            assert!(
                cx.debug_bounds("swap-destination-account-problem")
                    .is_some()
            );

            // Another network returns the destination to a new account. The source stays.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.select_form_account(Some(operation), window, cx);
                    swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            assert!(
                cx.debug_bounds("swap-account-select").is_some()
                    && cx.debug_bounds("swap-account-source").is_none()
                    && cx.debug_bounds("swap-account-destination").is_none(),
                "a Public address has one selector"
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    assert_eq!(chosen(swaps), (Some(operation), Some(destination)));
                    swaps.open_buy_picker(window, cx);
                    swaps.show_buy_picker_network(1, window, cx);
                    swaps.pick_buy_token(STUB_USDT, window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(form.network, None);
                    assert_eq!(form.destination_account, None);
                    assert_eq!(chosen(swaps), (Some(operation), None));
                });
                window.draw(cx).clear(cx);
            });
            assert!(
                cx.debug_bounds("swap-account-select").is_some()
                    && cx.debug_bounds("swap-account-source").is_none()
                    && cx.debug_bounds("swap-account-destination").is_none(),
                "a same-chain swap has one selector"
            );

            // Another session's form starts with a new account on each side.
            cx.update(|window, cx| {
                window.close_all_dialogs(cx);
                let fresh = root.update(cx, |root, cx| {
                    root.clear_private_swaps(cx);
                    root.ensure_private_swaps(window, cx);
                    root.private_swaps_view().unwrap()
                });
                assert!(swaps.read(cx).form.is_none());
                fresh.update(cx, |fresh, cx| {
                    fresh.open_new_form(STUB_USDC, window, cx);
                    let form = fresh.form.as_ref().unwrap();
                    assert!(form.operation.is_none() && form.destination_account.is_none());
                    assert!(form.reuse_use.is_none());
                });
            });
            runtime.block_on(session.stop()).unwrap();
        },
    );
}

/// A setup broadcaster's offer for `fee` of `token` on `chain_id`.
fn setup_offer(
    root: &Entity<WalletRoot>,
    chain_id: u64,
    token: Address,
    fee: U256,
    cx: &gpui::VisualTestContext,
) -> PublicBroadcasterCandidate {
    let delegate = root.read_with(cx, |root, _| {
        root.effective_chain_configs
            .get(chain_id)
            .unwrap()
            .accepted_executor_profile()
            .unwrap()
            .delegate()
    });
    PublicBroadcasterCandidate {
        chain_id,
        railgun_address: "setup-offer".into(),
        identifier: None,
        token,
        fee,
        fees_id: "setup-offer".into(),
        fee_expiration: std::time::SystemTime::now() + Duration::from_secs(60),
        reliability: 0.9,
        available_wallets: 1,
        version: "8.2.3".into(),
        relay_adapt: Address::ZERO,
        relay_adapt_7702: Some(delegate),
        required_poi_list_keys: Vec::new(),
        viewing_public_key: [1; 32],
        address_data: broadcaster_core::crypto::railgun::AddressData {
            master_public_key: U256::ONE,
            viewing_public_key: [1; 32],
        },
        fee_policy_status: wallet_ops::BroadcasterFeePolicyStatus::UnknownAnchor,
    }
}

/// Each account of a private Bridge swap needs setup exactly while it is a new one. The
/// review's setup fees, the fee limits and the accounts its approval binds follow the four
/// combinations, and an existing account's stale estimate is never approved. A broadcaster
/// offer that appears or expires on a reused side leaves the quote and its acknowledgements.
/// Reuse keeps the delivery and shield costs, and the high-cost acknowledgement.
#[gpui::test]
fn private_bridge_setups_follow_each_account(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    // The destination costs of `high_bridge_costs_require_swap_anyway`.
    stubs.set_across_fee_bps(300);
    stubs
        .across_relayer_gas_bps
        .store(200, std::sync::atomic::Ordering::Relaxed);
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, store, cx| {
            reusable_account(executors, operation);
            // A setup estimate on each network, as the form's routes would hold them.
            let (session, owner) = swaps.read_with(cx, |swaps, _| {
                (
                    Arc::clone(swaps.private_session().unwrap()),
                    Arc::clone(swaps.private_owner().unwrap()),
                )
            });
            owner.plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10).pow(U256::from(20)));
            let origin_offer =
                setup_offer(root, 1, STUB_USDC, U256::from(10).pow(U256::from(18)), cx);
            let origin_estimate = runtime
                .block_on(Box::pin(owner.estimate_swap_setup_fee(
                    &session,
                    None,
                    origin_offer.clone(),
                )))
                .unwrap();
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let polygon_owner = polygon.executor_owner().unwrap();
            polygon_owner
                .plan_swaps_from_note_for_tests(STUB_POLYGON_USDT, U256::from(100_000_000));
            let destination_offer = setup_offer(root, 137, STUB_POLYGON_USDT, U256::from(10), cx);
            let destination_estimate = runtime
                .block_on(Box::pin(polygon_owner.estimate_swap_setup_fee(
                    &polygon,
                    None,
                    destination_offer.clone(),
                )))
                .unwrap();
            let (polygon_store, destination) = reusable_polygon_account(root, cx);
            let account = Address::repeat_byte(3).to_checksum(None);
            // Each reused account as its review names it: its number, then its address.
            let numbered = |executors: &ExecutorStore, operation| {
                let records = executors.records().unwrap();
                let index = records
                    .iter()
                    .find(|record| record.operation() == operation)
                    .unwrap()
                    .index();
                format!("#{index}")
            };
            let (own, reused) = (
                numbered(executors, operation),
                numbered(&polygon_store, destination),
            );
            let row = |summary: &SpendAuthorizationSummary, label: &str| {
                summary
                    .rows_for_test()
                    .into_iter()
                    .find(|(row, _)| row == label)
                    .map(|(_, value)| value)
            };
            let reuse_warnings = |summary: &SpendAuthorizationSummary| {
                summary
                    .warnings_for_test()
                    .into_iter()
                    .filter(|warning| warning.starts_with("Reusing"))
                    .collect::<Vec<_>>()
            };

            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    swaps.select_form_destination(Some(destination), window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            // The existing destination still costs its delivery and its shield fee.
            assert!(!private_delivery_cost(&review).is_zero());
            assert!(authorized_high_cost(&review).is_some());
            // The review names both accounts: a new one on Ethereum, and the reused one on
            // Polygon by its number and address, with its own warning and disclosure.
            swaps.read_with(cx, |swaps, cx| {
                let summary = swaps.swap_summary(&review, None, None, None, cx);
                assert_eq!(
                    row(&summary, "Source · Ethereum").as_deref(),
                    Some("New account")
                );
                assert_eq!(
                    row(&summary, "Destination · Polygon"),
                    Some(format!("{reused} · {account}"))
                );
                let warnings = reuse_warnings(&summary);
                assert_eq!(warnings.len(), 1, "{warnings:?}");
                assert!(
                    warnings[0].starts_with(&format!("Reusing {reused} on Polygon links")),
                    "{warnings:?}"
                );
                let (_, public, _) = summary.disclosure_for_test().unwrap();
                assert!(
                    public.contains(&format!(
                        "This swap reuses stealth account {reused} on Polygon"
                    )) && !public.contains("on Ethereum, so anyone"),
                    "{public}"
                );
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    assert_eq!(
                        form.review_problem(&review)
                            .map(|problem| problem.to_string()),
                        Some("Confirm Swap anyway to accept the high swap costs.".to_owned())
                    );
                    form.high_costs_acknowledged = true;

                    // Offers on Polygon come and go while its account is an existing one.
                    let mut offer = crate::root::tests::fee_row(137, STUB_POLYGON_USDT, "churn");
                    offer.relay_adapt_7702 = destination_offer.relay_adapt_7702;
                    for expiration in [
                        std::time::SystemTime::now() + Duration::from_secs(60),
                        std::time::UNIX_EPOCH,
                    ] {
                        offer.fee_expiration = expiration;
                        let _ = swaps.root.update(cx, |root, _| {
                            root.monitor_state.write().upsert_fee(offer.clone());
                        });
                        swaps.update_setup_route(cx);
                        swaps.refresh_destination_network(window, cx);
                        let form = swaps.form.as_ref().unwrap();
                        assert!(
                            matches!(&form.quote, QuoteState::Ready(current)
                                if Arc::ptr_eq(current, &review)),
                            "an offer on a reused side keeps the quote"
                        );
                        assert!(form.price_acknowledged && form.high_costs_acknowledged);
                        assert_eq!(form.review_problem(&review), None);
                        assert!(!form.destination_route.is_used());
                    }

                    for (source_existing, destination_existing) in
                        [(false, false), (true, false), (false, true), (true, true)]
                    {
                        swaps.select_form_account(source_existing.then_some(operation), window, cx);
                        swaps.select_form_destination(
                            destination_existing.then_some(destination),
                            window,
                            cx,
                        );
                        // Both routes hold an estimate, also one a side no longer needs.
                        let form = swaps.form.as_mut().unwrap();
                        form.price_acknowledged = true;
                        form.route.estimate = Some(origin_estimate.clone());
                        form.route.candidates = vec![origin_offer.clone()];
                        form.destination_route.estimate = Some(destination_estimate.clone());
                        form.destination_route.candidates = vec![destination_offer.clone()];
                        let form = swaps.form.as_ref().unwrap();
                        let combination = (source_existing, destination_existing);
                        // The broadcaster controls are those of the accounts that need setup.
                        assert_eq!(
                            (
                                swaps.setup_chain(form, SetupSide::Origin),
                                swaps.setup_chain(form, SetupSide::Destination),
                            ),
                            (
                                (!source_existing).then_some(1),
                                (!destination_existing).then_some(137),
                            ),
                            "{combination:?}"
                        );
                        if source_existing && destination_existing {
                            assert!(!swaps.reviews_setup(form), "no setup is reviewed");
                            continue;
                        }
                        let parts = swaps.setup_parts(form, &review, false).unwrap();
                        let fees = setup_fees(
                            1,
                            parts.origin.as_ref(),
                            parts.destination.as_ref().and_then(DestinationPlan::setup),
                        );
                        assert_eq!(
                            fees.iter().map(|fee| fee.chain_id).collect::<Vec<_>>(),
                            [
                                (!source_existing).then_some(1),
                                (!destination_existing).then_some(137),
                            ]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>(),
                            "{combination:?}"
                        );
                        let limit = |estimate: &ExecutorRecoveryFeeEstimate| {
                            default_public_broadcaster_fee_limit(estimate.fee_amount())
                        };
                        let bounds = &parts.approval.bounds;
                        assert_eq!(
                            (bounds.source_setup_fee, bounds.destination_setup_fee),
                            (
                                (!source_existing).then(|| limit(&origin_estimate)),
                                (!destination_existing).then(|| limit(&destination_estimate)),
                            ),
                            "{combination:?}"
                        );
                        let accounts = parts.approval.accounts.unwrap();
                        let account = Address::repeat_byte(3);
                        assert_eq!(
                            accounts.source,
                            SwapApprovedAccount {
                                address: source_existing.then_some(account),
                                setup: !source_existing,
                            },
                            "{combination:?}"
                        );
                        assert_eq!(
                            accounts.destination,
                            Some(SwapApprovedAccount {
                                address: destination_existing.then_some(account),
                                setup: !destination_existing,
                            }),
                            "{combination:?}"
                        );
                        assert_eq!(
                            parts.destination.as_ref().and_then(|plan| match plan {
                                DestinationPlan::Existing(account) => Some(account.operation),
                                DestinationPlan::Setup(_) => None,
                            }),
                            destination_existing.then_some(destination),
                        );
                    }
                });
            });

            // With both accounts existing ones, the swap is quoted for the chosen account on
            // the stub route again. Its review names each account and warns for each, and
            // Review opens the order's review at once: no setup is reviewed or waited for.
            let orderbook = stub_orderbook(&stubs, runtime);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.bridge_clients = Some(stub_bridge_clients(&stubs, &orderbook));
                    form.orderbook = Some(orderbook);
                    swaps.schedule_quote(window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let summary = swaps.swap_summary(&review, None, None, None, cx);
                    assert_eq!(
                        row(&summary, "Source · Ethereum"),
                        Some(format!("{own} · {account}"))
                    );
                    assert_eq!(
                        row(&summary, "Destination · Polygon"),
                        Some(format!("{reused} · {account}"))
                    );
                    let warnings = reuse_warnings(&summary);
                    assert!(
                        warnings.len() == 2
                            && warnings[0].starts_with(&format!("Reusing {own} on Ethereum"))
                            && warnings[1].starts_with(&format!("Reusing {reused} on Polygon")),
                        "{warnings:?}"
                    );
                    let (_, public, _) = summary.disclosure_for_test().unwrap();
                    for reuse in [
                        format!("This swap reuses stealth account {own} on Ethereum"),
                        format!("This swap reuses stealth account {reused} on Polygon"),
                    ] {
                        assert!(public.contains(&reuse), "{public}");
                    }
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    let draft = form.reuse_use;
                    swaps.form_primary(window, cx);
                    let pending = swaps
                        .pending_authorization
                        .as_ref()
                        .unwrap_or_else(|| panic!("{:?}", swaps.form.as_ref().unwrap().error));
                    let SwapAction::Order(approval) = &pending.action else {
                        panic!("both existing accounts go straight to the order's review");
                    };
                    assert!(approval.full_review);
                    assert_eq!(approval.swap_use, draft);
                    assert_eq!(
                        approval.pair_destination.map(|account| account.operation),
                        Some(destination)
                    );
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// The destination select works from the keyboard like any other select: the arrow keys open
/// it and move through its accounts, Enter chooses one, and focus is back on the select.
#[gpui::test]
fn private_bridge_destination_is_chosen_from_the_keyboard(cx: &mut TestAppContext) {
    use gpui::Focusable as _;
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            let session = start_polygon_session(root, &stubs, runtime, store, cx);
            let (_, destination) = reusable_polygon_account(root, cx);
            let select = cx.update(|window, cx| {
                let select = swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(form.destination_choice(), None, "a new account by default");
                    form.destination_select.clone().unwrap()
                });
                window.draw(cx).clear(cx);
                // As Tab does from the source select.
                select.update(cx, |select, cx| select.focus(window, cx));
                window.draw(cx).clear(cx);
                select
            });
            let focused = |cx: &mut gpui::VisualTestContext| {
                cx.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    select.read(cx).focus_handle(cx).is_focused(window)
                })
            };
            assert!(focused(cx));

            // Down opens the list on the selected new account, and again moves to the
            // existing one below it.
            cx.simulate_keystrokes("down");
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_keystrokes("down enter");
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    form.destination_choice().map(|account| account.operation),
                    Some(destination)
                );
                assert_eq!(form.operation, None, "the source stays a new account");
            });
            assert!(focused(cx), "focus returns to the select it was chosen in");
            runtime.block_on(session.stop()).unwrap();
        },
    );
}

/// A swap that reuses its own stealth account and sets up a new one on the destination network
/// is approved once. Its progress shows the source as reused and ready and only the new
/// account's setup as pending, and the approved new account resolving to its derived address
/// changes nothing that was approved. Once that setup is confirmed the approved order is
/// quoted again and placed through the confirm-only step, without another review.
#[gpui::test]
fn reused_source_places_its_approved_order_once_the_new_destination_is_set_up(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use gpui_kit::test::TestWindowExt;
    use wallet_ops::vault::{ExecutorNonceObservation, SwapDestinationClaim, SwapPairClaim};
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, store, cx| {
            reusable_account(executors, operation);
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let (polygon_store, existing) = reusable_polygon_account(root, cx);
            // The swap's terms as the form quotes them. Polygon is unfunded here, so the
            // quote is one for a delivery to its existing account.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    swaps.select_form_destination(Some(existing), window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            let SwapDelivery::Bridge(delivery) = review.plan().delivery() else {
                panic!("a Bridge delivery");
            };
            // The approval as the review saved it: the swap's own account is an existing
            // one, and the destination a new one, which has no address before it is derived.
            let (own, receiver) = (Address::repeat_byte(3), Address::repeat_byte(0x61));
            let mut approval = review
                .approval(review.suggested_private_minimum(), true)
                .unwrap();
            approval.delivery = SwapDelivery::Bridge(BridgeDelivery {
                receiver: Address::ZERO,
                ..delivery
            });
            approval.bounds.destination_setup_fee = Some(U256::from(50_000));
            let approved = SwapApprovedAccounts {
                source: SwapApprovedAccount {
                    address: Some(own),
                    setup: false,
                },
                destination: Some(SwapApprovedAccount {
                    address: None,
                    setup: true,
                }),
            };
            approval.accounts = Some(approved);
            let (origin_delegate, polygon_delegate) = root.read_with(cx, |root, _| {
                let delegate = |chain_id| {
                    root.effective_chain_configs
                        .get(chain_id)
                        .unwrap()
                        .accepted_executor_profile()
                        .unwrap()
                        .delegate()
                };
                (delegate(1), delegate(137))
            });
            let swap_use = SwapUseId::random().unwrap();
            let draft = PendingSwapOrder {
                previous_order: None,
                sell: review.plan().sell_token(),
                buy: review.plan().buy_token(),
                delivery: approval.delivery,
                amount: approval.bounds.spend_amount(),
                private_minimum: approval.bounds.private_minimum,
                slippage_bps: review.slippage_bps(),
                gas_share_bps: review.gas_share_bps(),
                valid_for: review.valid_for(),
                reuse_account: true,
                swap_use,
                started_at: now_unix(),
            };
            let destination = ExecutorOperationId::random().unwrap();
            executors
                .claim_swap_pair(SwapPairClaim {
                    id: swap_use,
                    source: SwapAccountChoice::Existing(operation),
                    delegate: origin_delegate,
                    purpose_summary: None,
                    assets: Vec::new(),
                    approval: approval.clone(),
                    destination: Some(SwapDestinationClaim {
                        chain_id: 137,
                        account: SwapAccountChoice::New(destination),
                        delegate: polygon_delegate,
                        destination_token: STUB_POLYGON_USDT,
                    }),
                })
                .unwrap();
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.tracking.entry(operation).or_default().pending_order = Some(draft);
                    let account = swaps
                        .labels(swaps.record(operation).unwrap(), cx)
                        .bridge
                        .unwrap()
                        .private
                        .unwrap()
                        .setups[1]
                        .account;
                    assert!(
                        account.is_none(),
                        "an underived account has no address to copy"
                    );
                });
            });
            // The new account is derived and its setup is sent.
            polygon_store.bind_address(destination, receiver).unwrap();
            approval.delivery = SwapDelivery::Bridge(BridgeDelivery {
                receiver,
                ..delivery
            });
            executors
                .record_swap_approval(operation, swap_use, approval)
                .unwrap();
            let setup_hash = B256::repeat_byte(0x44);
            record_setup(
                &polygon_store,
                destination,
                polygon_delegate,
                setup_hash,
                Vec::new(),
            );

            let orderbook = stub_orderbook(&stubs, runtime);
            cx.update(gpui_component::WindowExt::close_all_dialogs);
            cx.run_until_parked();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.form = None;
                    swaps.reload_records();
                    swaps.reload_destinations(cx);
                    // What the approved setup's submission leaves in this session.
                    let tracking = swaps.tracking.entry(operation).or_default();
                    tracking.pending_order = Some(draft);
                    tracking.auto_place = true;
                    tracking.bridge_clients = Some(stub_bridge_clients(&stubs, &orderbook));
                    tracking.orderbook = Some(orderbook);
                    let record = swaps.record(operation).unwrap();
                    assert_eq!(swaps.progress_stage(record), SwapStage::SetupPending);
                    let setups = swaps
                        .labels(record, cx)
                        .bridge
                        .unwrap()
                        .private
                        .unwrap()
                        .setups;
                    assert_eq!(
                        (setups[0].reused, setups[0].progress),
                        (true, SwapSetupProgress::Done),
                        "the source is reused and ready"
                    );
                    assert_eq!(
                        (setups[1].reused, setups[1].progress),
                        (false, SwapSetupProgress::Pending),
                        "only the new destination's setup is pending"
                    );
                    assert_eq!(
                        setups[1].account.map(|account| account.address),
                        Some(receiver),
                        "the pending draft shows the destination the preparation bound"
                    );
                    // The approved new account took its derived address: the accounts the
                    // swap holds are the approved ones, and nothing asks for a review.
                    let bound = swaps.bound_accounts(record, swap_use).unwrap();
                    assert_eq!(
                        bound.destination,
                        Some(SwapApprovedAccount {
                            address: Some(receiver),
                            setup: true,
                        })
                    );
                    assert!(approved.admits((own, false), Some((receiver, true))));
                    assert!(swaps.reapproval.is_none());
                    swaps.continue_approved_swaps(window, cx);
                    assert!(
                        swaps.job.is_none() && swaps.pending_authorization.is_none(),
                        "the order waits for the destination setup"
                    );
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            cx.update(|window, cx| {
                window.click(
                    SharedString::from(format!(
                        "swap-step-account-{}-copy",
                        receiver.to_checksum(None)
                    )),
                    cx,
                );
            });
            assert_eq!(
                cx.read_from_clipboard().unwrap().text(),
                Some(receiver.to_checksum(None))
            );
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    // A session draft from another use must not borrow this preparation's
                    // destination account, even while it has no order of its own.
                    swaps.tracking.entry(operation).or_default().pending_order =
                        Some(PendingSwapOrder {
                            swap_use: SwapUseId::random().unwrap(),
                            ..draft
                        });
                    let account = swaps
                        .labels(swaps.record(operation).unwrap(), cx)
                        .bridge
                        .unwrap()
                        .private
                        .unwrap()
                        .setups[1]
                        .account;
                    assert!(
                        account.is_none(),
                        "another use does not resolve the placeholder"
                    );
                    swaps.tracking.entry(operation).or_default().pending_order = Some(draft);
                });
            });

            // The setup is confirmed: the order follows by itself.
            confirm_setup(
                &polygon_store,
                destination,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(12, B256::repeat_byte(12)),
                    U256::ONE,
                ),
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_destinations(cx);
                    swaps.continue_approved_swaps(window, cx);
                    assert_eq!(
                        swaps.job.as_ref().map(|job| job.kind),
                        Some(SwapJobKind::Requote)
                    );
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.job.is_none())
            });
            swaps.read_with(cx, |swaps, cx| {
                let pending = swaps.pending_authorization.as_ref().unwrap_or_else(|| {
                    panic!(
                        "{:?} {:?}",
                        swaps.reapproval, swaps.tracking[&operation].error
                    )
                });
                let SwapAction::Order(placed) = &pending.action else {
                    panic!("the approved order's confirm step");
                };
                assert!(!placed.full_review, "no second review");
                assert_eq!(placed.swap_use, Some(swap_use));
                assert!(swaps.reapproval.is_none() && swaps.form.is_none());
                // The confirm step names both accounts by the addresses the approval binds.
                let rows = swaps.place_summary(placed, cx).unwrap().rows_for_test();
                for (label, address) in [
                    ("Source · Ethereum", own),
                    ("Destination · Polygon", receiver),
                ] {
                    let value = rows.iter().find(|(row, _)| row == label);
                    assert!(
                        value.is_some_and(|(_, value)| {
                            value.starts_with('#') && value.ends_with(&address.to_checksum(None))
                        }),
                        "{rows:?}"
                    );
                }
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// After a restart only the records tell of a swap whose accounts are reserved and that has
/// no order yet. It is listed in My orders and counted on the Private tab, and its detail
/// offers Place order and Cancel preparation. With both accounts ready, Place order checks the
/// approved terms again instead of opening the form. The form still restores the reserved pair
/// and the saved terms, where Review opens the order's review. Cancel preparation releases both
/// existing accounts, which can be chosen again and aren't retired. When the swap set up a
/// new destination account whose setup was sent, the cancellation reports that account as
/// unresolved and its guards stay.
#[gpui::test]
fn a_prepared_swap_is_listed_resumed_and_cancelled_after_a_restart(cx: &mut TestAppContext) {
    use alloy::primitives::B256;
    use wallet_ops::vault::{
        ExecutorInputIdentity, SwapApprovalTokens, SwapDestinationClaim, SwapPairClaim,
        SwapUseRelease,
    };
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, _, executors, operation, runtime, store, cx| {
            reusable_account(executors, operation);
            // Hidden, so that only a prepared swap shows the account on the Private tab.
            executors.set_hidden(operation, true).unwrap();
            cx.update(|_, cx| root.update(cx, |root, _| enable_stub_chain(root, &stubs, 137)));
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let polygon_owner = polygon.executor_owner().unwrap();
            let (polygon_store, existing) = reusable_polygon_account(root, cx);
            let (origin_delegate, polygon_delegate) = root.read_with(cx, |root, _| {
                let delegate = |chain_id| {
                    root.effective_chain_configs
                        .get(chain_id)
                        .unwrap()
                        .accepted_executor_profile()
                        .unwrap()
                        .delegate()
                };
                (delegate(1), delegate(137))
            });
            let own = Address::repeat_byte(3);
            // The approval a preparation saves with its claim: 1 USDC for USDT on Polygon, to
            // the account at `receiver` there, which the swap sets up when `new_destination`.
            let prepared = |receiver: Address, new_destination: bool| {
                let mut approval = test_approval();
                approval.bounds.valid_for_secs = Some(600);
                approval.bounds.gas_share_bps = Some(wallet_ops::cow::GAS_SHARE_BALANCED_BPS);
                approval.bounds.destination_minimum = Some(BRIDGE_MINIMUM);
                approval.bounds.destination_setup_fee =
                    new_destination.then_some(U256::from(50_000));
                approval.delivery = SwapDelivery::Bridge(BridgeDelivery {
                    provider: BridgeProvider::Across,
                    destination_chain: 137,
                    receiver,
                    destination_token: STUB_POLYGON_USDT,
                    surplus: BridgeSurplus::Reshield,
                    private: Some(BridgePrivateDelivery {
                        on_shield_failure: BridgeShieldFailure::default(),
                    }),
                });
                approval.tokens = Some(SwapApprovalTokens {
                    sell: STUB_USDC,
                    buy: STUB_USDT,
                });
                approval.accounts = Some(SwapApprovedAccounts {
                    source: SwapApprovedAccount {
                        address: Some(own),
                        setup: false,
                    },
                    destination: Some(SwapApprovedAccount {
                        address: Some(receiver),
                        setup: new_destination,
                    }),
                });
                approval
            };
            let claim = |id, destination, approval| {
                executors
                    .claim_swap_pair(SwapPairClaim {
                        id,
                        source: SwapAccountChoice::Existing(operation),
                        delegate: origin_delegate,
                        purpose_summary: None,
                        assets: Vec::new(),
                        approval,
                        destination: Some(SwapDestinationClaim {
                            chain_id: 137,
                            account: destination,
                            delegate: polygon_delegate,
                            destination_token: STUB_POLYGON_USDT,
                        }),
                    })
                    .unwrap();
            };
            let record_of = |executors: &ExecutorStore, operation| {
                let records = executors.records().unwrap();
                records
                    .into_iter()
                    .find(|record| record.operation() == operation)
                    .unwrap()
            };
            // Whether each account is offered to another swap, from local records.
            let offered = |swaps: &Entity<PrivateSwapsView>, cx: &gpui::VisualTestContext| {
                let source = swaps.read_with(cx, |swaps, _| {
                    let candidates = swaps
                        .private_owner()
                        .unwrap()
                        .swap_account_candidates()
                        .unwrap();
                    candidates
                        .iter()
                        .any(|candidate| candidate.operation() == operation)
                });
                let candidates = polygon_owner
                    .swap_destination_candidates(STUB_POLYGON_USDT)
                    .unwrap();
                let destination = candidates
                    .iter()
                    .any(|candidate| candidate.operation() == existing);
                (source, destination)
            };
            // Confirm the alert through its normal keyboard action.
            let cancel_preparation = |cx: &mut gpui::VisualTestContext| {
                let cancel = cx.debug_bounds("swap-progress-cancel-preparation").unwrap();
                cx.simulate_click(cancel.center(), gpui::Modifiers::none());
                cx.simulate_keystrokes("enter");
                cx.run_until_parked();
            };

            // Both accounts are existing ones, claimed before the restart.
            let first = SwapUseId::random().unwrap();
            claim(
                first,
                SwapAccountChoice::Existing(existing),
                prepared(own, false),
            );
            let swaps = restarted_swaps(
                root,
                &stubs,
                runtime,
                operation,
                wallet_ops::SwapSetupStatus::Pending,
                cx,
            );
            assert_eq!(offered(&swaps, cx), (false, false), "both are reserved");
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_destinations(cx);
                    let record = swaps.record(operation).unwrap();
                    // Only the records tell of the swap: this session holds no draft of it.
                    assert!(swaps.tracking[&operation].pending_order.is_none());
                    assert_eq!(
                        swaps.pending_order(record).map(|pending| pending.swap_use),
                        Some(first)
                    );
                    assert!(swaps.has_shown_swaps(), "counted on the Private tab");
                    assert_eq!(swaps.open_order_count(cx), 1);
                    let rows = swaps.order_rows_for_test(cx);
                    assert!(
                        rows.len() == 1 && rows[0].0.contains("to private balance on Polygon"),
                        "{rows:?}"
                    );
                    // Both accounts are reused and ready: nothing is set up or pending.
                    assert_eq!(swaps.progress_stage(record), SwapStage::Ready);
                    let setups = swaps
                        .labels(record, cx)
                        .bridge
                        .unwrap()
                        .private
                        .unwrap()
                        .setups;
                    assert!(setups.iter().all(|account| {
                        account.reused && account.progress == SwapSetupProgress::Done
                    }));
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            assert!(
                cx.debug_bounds("swap-progress-cancel-preparation")
                    .is_some()
            );
            assert!(cx.debug_bounds("swap-progress-stop").is_none());
            let place = cx.debug_bounds("swap-progress-continue").unwrap();
            cx.simulate_click(place.center(), gpui::Modifiers::none());
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    // Both accounts are ready, so the approved order is quoted again.
                    let job = swaps
                        .job
                        .take()
                        .expect("a ready prepared swap places its approved order");
                    assert_eq!(job.kind, SwapJobKind::Requote);
                    assert!(swaps.form.is_none());
                    job.abort.abort();
                    swaps.job_revision += 1;
                    swaps.open_existing_form(operation, window, cx);
                    assert_eq!(
                        swaps.dialog.as_ref().map(|dialog| dialog.view),
                        Some(dialog::SwapDialogView::Form)
                    );
                    let form = swaps
                        .form
                        .as_ref()
                        .expect("the form restores the prepared swap");
                    assert_eq!(form.operation, Some(operation));
                    assert!(form.reuse_account);
                    assert_eq!(form.reuse_use, Some(first));
                    assert_eq!((form.sell, form.network), (STUB_USDC, Some(137)));
                    assert_eq!(form.receive_to, ReceiveTo::PrivateBalance);
                    // The pair the swap reserved comes back with its terms.
                    assert_eq!(swaps.destination_operation(form), Some(existing));
                    assert_eq!(
                        swaps
                            .reserved_delivery(form)
                            .map(|delivery| delivery.receiver),
                        Some(own)
                    );
                    // Neither account needs setup, so Review opens the order's review.
                    assert_eq!(swaps.form_mode(form), FormMode::Order);
                    assert!(!swaps.reviews_setup(form));
                });
            });
            assert!(
                !pay_from_menu_opens(cx),
                "a swap that keeps its stealth account keeps its payer"
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| swaps.show_detail(operation, window, cx));
                window.draw(cx).clear(cx);
            });

            cancel_preparation(cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let cancelled = swaps
                        .cancelled
                        .as_ref()
                        .expect("the detail reports the cancellation");
                    assert!(
                        cancelled.accounts.len() == 2
                            && cancelled.accounts.iter().all(|account| {
                                !account.fresh
                                    && account.release == SwapUseRelease::Released
                                    && account.outcome().starts_with("Released.")
                            })
                    );
                    assert!(
                        swaps
                            .pending_order(swaps.record(operation).unwrap())
                            .is_none()
                    );
                    assert!(!swaps.has_shown_swaps());
                    assert_eq!(swaps.open_order_count(cx), 0);
                });
                window.draw(cx).clear(cx);
            });
            assert!(cx.debug_bounds("swap-cancelled-source").is_some());
            assert!(cx.debug_bounds("swap-cancelled-destination").is_some());
            assert!(
                cx.debug_bounds("swap-progress-cancel-preparation")
                    .is_none()
            );
            // Native cancellation released both accounts. Neither is retired or stopped, and
            // each is offered again.
            for (store, account) in [(executors, operation), (&polygon_store, existing)] {
                let record = record_of(store, account);
                assert_eq!(record.active_swap_use(), None);
                assert!(record.swap_use(first).unwrap().is_stopped());
                assert!(!record.is_retired() && !record.is_swap_setup_stopped());
            }
            assert_eq!(offered(&swaps, cx), (true, true));

            // Another preparation sets up a new destination account, whose setup was sent.
            let second = SwapUseId::random().unwrap();
            let fresh = ExecutorOperationId::random().unwrap();
            let receiver = Address::repeat_byte(0x61);
            claim(
                second,
                SwapAccountChoice::New(fresh),
                prepared(receiver, true),
            );
            polygon_store.bind_address(fresh, receiver).unwrap();
            // The existing destination's setup used a different fee note.
            let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
                "tree": 4, "position": 16198, "commitment": "0x2"
            }))
            .unwrap();
            let issued = record_setup(
                &polygon_store,
                fresh,
                polygon_delegate,
                B256::repeat_byte(0x44),
                vec![input],
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.reload_destinations(cx);
                    let record = swaps.record(operation).unwrap();
                    assert_eq!(
                        swaps.pending_order(record).map(|pending| pending.swap_use),
                        Some(second)
                    );
                    assert_eq!(swaps.progress_stage(record), SwapStage::SetupPending);
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            cancel_preparation(cx);
            swaps.read_with(cx, |swaps, _| {
                let cancelled = swaps
                    .cancelled
                    .as_ref()
                    .expect("the detail reports the cancellation");
                let [source, destination] = cancelled.accounts.as_slice() else {
                    panic!("both accounts are reported");
                };
                assert!(!source.fresh && source.release == SwapUseRelease::Released);
                assert!(
                    destination.fresh
                        && destination.release == SwapUseRelease::IssuedWorkRemains
                        && destination.outcome().starts_with("Unresolved."),
                    "the sent setup is still unresolved"
                );
            });
            // The sent setup keeps its payload and the fee notes it reserves. The existing
            // source is released as before.
            let kept = record_of(&polygon_store, fresh);
            assert!(kept.is_swap_setup_stopped());
            assert_eq!(kept.issued().len(), 1);
            assert_eq!(kept.reserved_inputs(), issued.reserved_inputs());
            let source = record_of(executors, operation);
            assert_eq!(source.active_swap_use(), None);
            assert!(!source.is_retired() && !source.is_swap_setup_stopped());

            // Reopening from a new view must reconstruct cancellation from the records,
            // including the destination's outstanding setup and its retained input guard.
            let swaps = cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    root.clear_private_swaps(cx);
                    root.ensure_private_swaps(window, cx);
                    root.private_swaps_view().unwrap()
                })
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_destinations(cx);
                    assert!(swaps.cancelled.is_none());
                    let rows = swaps.order_rows_for_test(cx);
                    assert_eq!(rows.len(), 2, "both cancelled uses remain in history");
                    assert_eq!(swaps.open_order_count(cx), 0);
                    let identity = model::SwapIdentity {
                        operation,
                        swap_use: second,
                    };
                    let cancelled = swaps.cancelled_use(identity, cx).unwrap();
                    assert!(cancelled.needs_attention());
                    assert_eq!(
                        cancelled.accounts[1].release,
                        SwapUseRelease::IssuedWorkRemains
                    );
                    swaps.show_view(
                        dialog::SwapDialogView::CancelledPreparation(identity),
                        window,
                        cx,
                    );
                });
                window.draw(cx).clear(cx);
            });
            assert!(cx.debug_bounds("swap-cancelled-destination").is_some());

            // The same actions also belong to a prepared swap whose source is new.
            let third = SwapUseId::random().unwrap();
            let fresh_source = ExecutorOperationId::random().unwrap();
            let mut approval = prepared(own, false);
            approval.bounds.source_setup_fee = Some(U256::from(50_000));
            approval.accounts.as_mut().unwrap().source = SwapApprovedAccount {
                address: None,
                setup: true,
            };
            executors
                .claim_swap_pair(SwapPairClaim {
                    id: third,
                    source: SwapAccountChoice::New(fresh_source),
                    delegate: origin_delegate,
                    purpose_summary: Some("Private swap".into()),
                    assets: Vec::new(),
                    approval,
                    destination: Some(SwapDestinationClaim {
                        chain_id: 137,
                        account: SwapAccountChoice::Existing(existing),
                        delegate: polygon_delegate,
                        destination_token: STUB_POLYGON_USDT,
                    }),
                })
                .unwrap();
            executors
                .bind_address(fresh_source, Address::repeat_byte(0x73))
                .unwrap();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.reload_destinations(cx);
                    swaps.show_detail(fresh_source, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            let resume = cx.debug_bounds("swap-progress-continue").unwrap();
            cx.simulate_click(resume.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            let (session, owner) = swaps.read_with(cx, |swaps, _| {
                (
                    Arc::clone(swaps.private_session().unwrap()),
                    Arc::clone(swaps.private_owner().unwrap()),
                )
            });
            let offer = setup_offer(root, 1, STUB_USDC, U256::from(10), cx);
            let estimate = runtime
                .block_on(Box::pin(owner.estimate_swap_setup_fee(
                    &session,
                    Some(fresh_source),
                    offer.clone(),
                )))
                .unwrap();
            let orderbook = stub_orderbook(&stubs, runtime);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.orderbook = Some(orderbook.clone());
                    form.bridge_clients = Some(stub_bridge_clients(&stubs, &orderbook));
                    // Reopening queued its provider lookup before the fixture installed the
                    // stub clients. Reload that lookup on the stub route before asking for a quote.
                    form.bridge.routes_tasks.clear();
                    form.bridge.routes.clear();
                    swaps.load_bridge_routes(window, cx);
                    swaps.schedule_quote(window, cx);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| {
                    swaps
                        .form
                        .as_ref()
                        .unwrap()
                        .bridge
                        .routes
                        .contains_key(&(STUB_USDC, 137))
                })
            });
            ready_review(&swaps, runtime, cx);
            // This review never sends a setup. A disabled client supplies the reviewed action's
            // transport handle without starting networking or duplicating proof fixtures.
            let waku = root.read_with(cx, |root, _| {
                let mut config = root.waku_config.clone();
                config.network.mode = broadcaster_monitor_waku::RelayNetworkMode::Proxy;
                config.build_client().unwrap()
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.route.estimate = Some(estimate);
                    form.route.candidates = vec![offer];
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    let (approval, _) = swaps.setup_approval(true, Some(waku)).unwrap();
                    // A reopened setup's review reaches the same guarded persistence boundary
                    // as submission, before any setup payload is prepared or broadcast.
                    swaps
                        .private_owner()
                        .unwrap()
                        .record_swap_approval(
                            approval.operation,
                            approval
                                .swap_use
                                .unwrap_or_else(|| SwapUseId::first(approval.operation)),
                            approval.approval,
                        )
                        .expect("the reopened setup review still approves its claimed swap use");
                    swaps.reload_records();
                    swaps.show_detail(fresh_source, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            cancel_preparation(cx);
            assert!(
                record_of(executors, fresh_source)
                    .swap_use(third)
                    .unwrap()
                    .is_stopped()
            );
            assert_eq!(record_of(&polygon_store, existing).active_swap_use(), None);

            // Cancellation can precede derivation. The loaded destination record is still
            // known by its operation even while the approved receiver is a placeholder.
            let unbound = SwapUseId::random().unwrap();
            let unbound_destination = ExecutorOperationId::random().unwrap();
            claim(
                unbound,
                SwapAccountChoice::New(unbound_destination),
                prepared(Address::ZERO, true),
            );
            executors.cancel_swap_use(operation, unbound).unwrap();
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.reload_destinations(cx);
                    let cancelled = swaps
                        .cancelled_use(
                            model::SwapIdentity {
                                operation,
                                swap_use: unbound,
                            },
                            cx,
                        )
                        .unwrap();
                    assert_eq!(cancelled.accounts.len(), 2);
                    assert!(
                        !cancelled.needs_attention(),
                        "both loaded accounts stopped before signing anything"
                    );
                });
            });

            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// A private Bridge order's fill, verified as shielded, consumed its destination account's
/// execution nonce, which only a read of that account records. Once the delivery is verified,
/// the wallet has the destination network's own owner read the account at its confirmed
/// block. With that nonce recorded, local records offer the account as a destination again.
#[gpui::test]
fn a_shielded_delivery_reads_its_destination_account_which_is_then_offered_again(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::{ExecutorNonceObservation, SwapBridgeOutcome, SwapDestinationOutcome};
    const POLYGON_RPC: &str = "/rpc-polygon";

    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, origin, _, runtime, store, cx| {
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    enable_stub_chain(root, &stubs, 137);
                    // Polygon's requests arrive on their own path, apart from Ethereum's.
                    let mut polygon = root.effective_chain_configs.get(137).unwrap().clone();
                    polygon.rpc_route = wallet_ops::RpcChainRoute::new(
                        137,
                        vec![stubs.url.join("rpc-polygon").unwrap()],
                    );
                    root.effective_chain_configs = root
                        .effective_chain_configs
                        .clone()
                        .into_values()
                        .filter(|chain| chain.chain_id != 137)
                        .chain(std::iter::once(polygon))
                        .collect();
                });
            });
            let session = start_polygon_session(root, &stubs, runtime, store, cx);
            let polygon_owner = session.executor_owner().unwrap();
            let (db, view, delegate, origin_delegate) = root.read_with(cx, |root, _| {
                let delegate = |chain_id| {
                    root.effective_chain_configs
                        .get(chain_id)
                        .unwrap()
                        .accepted_executor_profile()
                        .unwrap()
                        .delegate()
                };
                (
                    root.vault_store.as_ref().unwrap().db(),
                    root.view_session.clone().unwrap(),
                    delegate(137),
                    delegate(1),
                )
            });
            let destination = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
            let PlacedPrivateBridge {
                operation,
                destination_operation,
                uid,
                origin_observed: observed,
                destination_observed: signed,
                ..
            } = placed_private_bridge(
                origin,
                &destination,
                origin_delegate,
                delegate,
                STUB_POLYGON_USDC,
            );
            origin
                .record_swap_observations(
                    operation,
                    uid,
                    bridge_observations(observed, Some(U256::from(7)), None),
                )
                .unwrap();
            // Polygon's confirmed block is known, as it is once its session has synced.
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    let Some(ChainUtxoState::Ready { sync_tip, .. }) =
                        root.chain_states.get_mut(&137)
                    else {
                        panic!("ready fixture");
                    };
                    sync_tip.head_block = Some(1_000);
                });
            });
            let account = || {
                let records = destination.records().unwrap();
                records
                    .into_iter()
                    .find(|record| record.operation() == destination_operation)
                    .unwrap()
            };
            let offered = || {
                let candidates = polygon_owner
                    .swap_destination_candidates(STUB_POLYGON_USDC)
                    .unwrap();
                candidates
                    .iter()
                    .any(|candidate| candidate.operation() == destination_operation)
            };
            assert!(!offered(), "its delivery isn't verified yet");
            assert_eq!(account().nonce_observation(), Some(signed));

            // A status check records the verified fill, which ran the shield.
            let requests = stubs.rpc_requests_on(POLYGON_RPC);
            let block = BlockNumHash::new(25, B256::repeat_byte(25));
            let transaction_hash = B256::repeat_byte(26);
            let writer = ExecutorStore::new(db, view, 1).unwrap();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.start_job(
                        operation,
                        SwapJobKind::Check,
                        async move {
                            writer.record_swap_bridge_outcome(
                                operation,
                                uid,
                                SwapBridgeOutcome::DeliveredVerified {
                                    block,
                                    transaction_hash,
                                    output_amount: BRIDGE_MINIMUM,
                                    shielded: true,
                                },
                            )?;
                            eyre::Ok(())
                        },
                        |_, (), _, _| {},
                        window,
                        cx,
                    );
                });
            });
            // The destination account's delivery is settled, and then Polygon's owner reads
            // the account: the read starts from an invalidated observation and asks
            // Polygon's own endpoint.
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.job.is_none())
                    && account().nonce_observation().is_none()
                    && stubs.rpc_requests_on(POLYGON_RPC) > requests
            });
            assert_eq!(
                account().swap_destination().unwrap().outcome,
                Some(SwapDestinationOutcome::Shielded {
                    block,
                    transaction_hash
                })
            );
            // The stub endpoint serves no account state, so nothing is recorded yet.
            assert!(!offered());
            // What that read records once the endpoint answers: the nonce the shield consumed.
            confirm_setup(
                &destination,
                destination_operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(990, B256::repeat_byte(99)),
                    U256::from(2),
                ),
            );
            assert!(offered(), "local records offer the delivered account again");
            runtime.block_on(session.stop()).unwrap();
        },
    );
}

/// A new swap of 1 USDC with the stub providers on its route and Polygon enabled with the
/// stub's RPC. Polygon's session is still loading, at 62%, so opening the picker for Private
/// balance starts no session.
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
            root.chain_states.insert(
                137,
                ChainUtxoState::Loading {
                    progress: Some(SyncProgressUpdate::new(
                        wallet_ops::SyncProgressStage::IndexingUtxos,
                        0,
                        62,
                        100,
                    )),
                },
            );
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

/// Whether the open picker lists Polygon as a network that can be picked.
fn picker_polygon(swaps: &PrivateSwapsView, cx: &App) -> NetworkAvailability {
    let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
    content
        .networks
        .iter()
        .find(|network| network.chain_id == 137)
        .unwrap()
        .availability
}

/// The picker's switch and the form's Receive to are one setting, and it decides which
/// networks can be picked. Switching the form to Private balance keeps a network picked for a
/// Public address: the form says under Receive to why it can't quote, and the picker falls
/// back to the swap's own network without changing the form's.
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
                NetworkAvailability::Syncing(Some(62)),
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
            assert_eq!((form.picker.network, form.network), (137, None));
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
        assert!(cx.debug_bounds("swap-receive-to-problem").is_some());

        // The picker follows the form's Receive to, and lists the swap's own network.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_buy_picker(window, cx);
                let form = swaps.form.as_ref().unwrap();
                let content = swaps.buy_picker_content(form, cx);
                assert_eq!(content.receive_to, ReceiveTo::PrivateBalance);
                assert_eq!((content.network, form.network), (1, Some(137)));
                assert_eq!(
                    picker_polygon(swaps, cx),
                    NetworkAvailability::Syncing(Some(62))
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
            swaps.read_with(cx, |swaps, _| swaps.form.as_ref().unwrap().picker.network)
        };
        let listed = |cx: &mut gpui::VisualTestContext| {
            draw(cx);
            networks.read_with(cx, |list, _| list.delegate().listed().collect::<Vec<_>>())
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

        // POL, USDC, then USDT.
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
                let summary = swaps.swap_summary(&review, None, None, None, cx);
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
                    full_review: false,
                    swap_use: None,
                    pair_destination: None,
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
                    root.chain_states
                        .insert(137, ChainUtxoState::Loading { progress: None });
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
                    let summary = swaps.swap_summary(&review, Some(&fees), None, None, cx);
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
                        full_review: false,
                        swap_use: None,
                        pair_destination: None,
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
                        review: Arc::clone(&review),
                        price_acknowledged: true,
                        orderbook: swaps.form.as_ref().unwrap().orderbook.clone().unwrap(),
                        bridge: None,
                        full_review: true,
                        swap_use: None,
                        pair_destination: None,
                    };
                    let summary = swaps.swap_summary(&review, None, None, None, cx);
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
            let summary = swaps.swap_summary(&review, None, None, None, cx);
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
                    full_review: false,
                    swap_use: None,
                    pair_destination: None,
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
                                .swap_summary(&review, None, None, None, cx)
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
            assert!(cx.debug_bounds("swap-gas-pending").is_some(), "{edit}");
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
        assert!(cx.debug_bounds("swap-gas-pending").is_none());
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
            let summary = swaps.swap_summary(&review, None, None, None, cx);
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

/// The destination minimum of the Bridge orders [`placed_swap`] places: 248.71 of a 6-decimal
/// token.
const BRIDGE_MINIMUM: U256 = U256::from_limbs([248_710_000, 0, 0, 0]);
/// The 1Click deposit address of the NEAR Intents orders [`placed_swap`] places.
const NEAR_DEPOSIT_ADDRESS: Address =
    alloy::primitives::address!("4444444444444444444444444444444444444444");

/// A traded Bridge order's observations at the account's `observed` block: its settlement
/// handed off to the bridge, with an Across `deposit_id` or none for NEAR Intents, and the
/// bridge reported `outcome` so far.
fn bridge_observations(
    observed: wallet_ops::vault::ExecutorNonceObservation,
    deposit_id: Option<U256>,
    outcome: Option<wallet_ops::vault::SwapBridgeOutcome>,
) -> wallet_ops::vault::SwapOrderObservations {
    use wallet_ops::vault::{
        SwapBridgeHandoff, SwapObservation, SwapOrderObservations, SwapTradeAmounts,
    };
    let seen = SwapObservation {
        block: observed.block(),
        transaction_hash: Some(alloy::primitives::B256::repeat_byte(40)),
    };
    SwapOrderObservations {
        pre_hook_executed: Some(seen),
        traded: Some(seen),
        trade_amounts: Some(SwapTradeAmounts {
            sell_amount: U256::from(100),
            buy_amount: U256::from(99),
            fee_amount: U256::ZERO,
            settlement_gas_used: None,
            settlement_effective_gas_price: None,
            executed_fee: None,
            executed_fee_token: None,
        }),
        delivered: Some(seen),
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: seen,
            deposit_id,
        }),
        bridge_outcome: outcome,
        ..Default::default()
    }
}

/// An Across Bridge swap names its destination and provider from the hand-off through each
/// outcome, and its detail shows the bridge's facts rather than a private-balance outcome.
/// Recover… for a refund waits until a check verified the refund and finds funds in the
/// stealth account, and recovery opens with that check's balance.
#[gpui::test]
fn across_bridge_swaps_show_the_hand_off_and_each_outcome(cx: &mut TestAppContext) {
    use crate::root::public_action::PublicActionStepStatus::{Done, Pending, Warning};
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::{
        BridgeDelivery, BridgeProvider, BridgeSurplus, SwapBridgeOutcome, SwapOrderObservations,
    };

    let receiver = Address::repeat_byte(0x51);
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                root.public_address_book = vec![cold_wallet_entry(receiver)];
            });
        });
        let delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver,
            destination_token: STUB_POLYGON_USDC,
            surplus: BridgeSurplus::Reshield,
            private: None,
        });
        let (uid, observed) = placed_swap(executors, operation, delivery);
        let handed_off = |outcome| bridge_observations(observed, Some(U256::from(7)), outcome);
        let verified = SwapBridgeOutcome::DeliveredVerified {
            block: BlockNumHash::new(71_904_233, B256::repeat_byte(60)),
            transaction_hash: B256::repeat_byte(61),
            output_amount: BRIDGE_MINIMUM,
            shielded: false,
        };
        // The post-hook didn't deposit: the trade's payout is still in the stealth account.
        let not_sent = SwapOrderObservations {
            delivered: None,
            bridge_handoff: None,
            undelivered: handed_off(None).traded,
            ..handed_off(None)
        };
        for (observations, state, last, status) in [
            (
                handed_off(None),
                SwapOrderState::Bridging,
                ("Delivered on Polygon", Pending),
                "Sent to the bridge",
            ),
            // A delivery only Across reported is final too, and isn't labelled verified.
            (
                handed_off(Some(SwapBridgeOutcome::DeliveredReported {
                    amount_out: None,
                    transaction_hash: None,
                })),
                SwapOrderState::Done,
                ("Delivered on Polygon · reported by Across", Done),
                "Delivered · reported by Across",
            ),
            (
                handed_off(Some(verified)),
                SwapOrderState::Done,
                ("Delivered on Polygon · verified", Done),
                "Delivered",
            ),
            (
                not_sent,
                SwapOrderState::NotDelivered,
                ("Not sent to the bridge", Warning),
                "Needs recovery",
            ),
            (
                handed_off(Some(SwapBridgeOutcome::Refunding)),
                SwapOrderState::Refunding,
                ("Refunding on Ethereum", Warning),
                "Refunding",
            ),
        ] {
            executors
                .record_swap_observations(operation, uid, observations)
                .unwrap();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    let record = swaps.record(operation).unwrap();
                    let stage = swaps.stage(record);
                    assert_eq!(stage, SwapStage::Order(state));
                    let labels = swaps.labels(record, cx);
                    let steps = model::swap_steps(stage, &labels);
                    let shown = steps.last().unwrap();
                    assert_eq!((shown.label.as_str(), shown.status), last, "{state:?}");
                    // The swap's destination is another network, never the private balance.
                    let card = model::swap_card_line(stage, &labels);
                    assert!(
                        steps
                            .iter()
                            .flat_map(|step| [&step.label, &step.detail])
                            .chain([&card.title, &card.detail])
                            .all(|text| !text.contains("private balance")),
                        "{state:?}"
                    );
                    // My orders adds the network and provider after the receiver.
                    let rows = swaps.order_rows_for_test(cx);
                    assert!(
                        rows.iter().any(|(meta, shown)| {
                            meta.ends_with(" · to Cold wallet on Polygon · Across")
                                && shown == status
                        }),
                        "{rows:?}"
                    );
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            assert!(cx.debug_bounds("swap-bridge-facts").is_some(), "{state:?}");
            assert!(cx.debug_bounds("swap-outcome").is_none(), "{state:?}");
            assert!(
                cx.debug_bounds("swap-detail-receiver").is_some(),
                "{state:?}"
            );
        }
        // Delivered, the swap is labelled with the verified amount on Polygon.
        executors
            .record_swap_observations(operation, uid, handed_off(Some(verified)))
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert_eq!(
                    swaps.labels(record, cx).received,
                    Some(swaps.network_token_amount(137, STUB_POLYGON_USDC, BRIDGE_MINIMUM, cx))
                );
            });
        });

        // A refund is recovered only once an explicit check finds it in the stealth account.
        // Kept surplus can be there before Across refunds, so the check must also have verified
        // the refund.
        executors
            .record_swap_observations(
                operation,
                uid,
                handed_off(Some(SwapBridgeOutcome::Refunding)),
            )
            .unwrap();
        let refunding = SwapStage::Order(SwapOrderState::Refunding);
        press_swap_recover(swaps, operation, refunding, cx);
        assert!(cx.debug_bounds("stealth-recovery-form").is_none());
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| {
                swaps.tracking.entry(operation).or_default().stealth_balance =
                    Some((U256::from(99), observed.block()));
            });
        });
        press_swap_recover(swaps, operation, refunding, cx);
        assert!(cx.debug_bounds("stealth-recovery-form").is_none());
        executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    bridge_refund: Some(wallet_ops::vault::SwapObservation {
                        block: observed.block(),
                        transaction_hash: Some(B256::repeat_byte(62)),
                    }),
                    ..handed_off(Some(SwapBridgeOutcome::Refunding))
                },
            )
            .unwrap();
        press_swap_recover(swaps, operation, refunding, cx);
        assert!(cx.debug_bounds("stealth-recovery-form").is_some());
    });
}

/// Across can keep the swap's surplus in the stealth account, and it can exceed the deposit.
/// Recovering it while the deposit is bridging, or before Across's refund, doesn't complete the
/// swap once the deposit refunds; only a recovery after the verified refund does.
#[gpui::test]
fn an_across_refund_is_recovered_only_after_its_verified_refund(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::{
        Call, CommitmentPreimage, RelayAdapt7702, ShieldCiphertext, ShieldRequest, TokenData,
        shieldCall,
    };
    use wallet_ops::vault::{
        BridgeDelivery, BridgeProvider, BridgeSurplus, ExecutorNonceObservation,
        ExecutorPayloadContext, ExecutorPayloadPurpose, IssuedExecutorPayload, SwapBridgeOutcome,
        SwapObservation, SwapOrderObservations,
    };

    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver: Address::repeat_byte(0x51),
            destination_token: STUB_POLYGON_USDC,
            surplus: BridgeSurplus::KeepInAccount,
            private: None,
        });
        let (uid, observed) = placed_swap(executors, operation, delivery);
        let record = || {
            executors
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap()
        };
        let setup = record().issued()[0].clone();
        let source = record().address().unwrap();
        let at = |number: u64| {
            BlockNumHash::new(number, B256::repeat_byte(u8::try_from(number).unwrap()))
        };
        // A recovery batch signed at `nonce`, the only action there, with the account read
        // past it. Returns its shield request with `number`, the block that shield is
        // received in.
        let recover = |nonce: u64, number: u64| {
            let before = ExecutorNonceObservation::new(at(number - 1), U256::from(nonce));
            executors.record_account_read(operation, before).unwrap();
            let salt = 0x70 + u8::try_from(nonce).unwrap();
            let request = ShieldRequest {
                preimage: CommitmentPreimage {
                    npk: B256::repeat_byte(salt),
                    token: TokenData::erc20(Address::repeat_byte(2)),
                    value: alloy::primitives::Uint::from(100_u64),
                },
                ciphertext: ShieldCiphertext {
                    encryptedBundle: [B256::ZERO; 3],
                    shieldKey: B256::ZERO,
                },
            };
            let calldata = RelayAdapt7702::multicallCall {
                _requireSuccess: true,
                _calls: vec![Call {
                    to: source,
                    data: shieldCall {
                        _shieldRequests: vec![request.clone()],
                    }
                    .abi_encode()
                    .into(),
                    value: U256::ZERO,
                }],
                _nonce: U256::from(nonce),
                _signature: Bytes::new(),
            }
            .abi_encode();
            executors
                .record_issued(
                    operation,
                    IssuedExecutorPayload::new(
                        U256::from(nonce),
                        setup.delegate(),
                        B256::repeat_byte(salt),
                        ExecutorPayloadPurpose::Recovery,
                        ExecutorPayloadContext::new(calldata.into(), before, Vec::new()),
                    ),
                )
                .unwrap();
            executors
                .record_account_read(
                    operation,
                    ExecutorNonceObservation::new(at(number + 1), U256::from(nonce + 1)),
                )
                .unwrap();
            (request, number)
        };
        let handed_off = |outcome, bridge_refund| SwapOrderObservations {
            bridge_refund,
            ..bridge_observations(observed, Some(U256::from(7)), outcome)
        };
        // This session doesn't sync, so the shields private sync would show stand in for it.
        let stage = |cx: &mut gpui::VisualTestContext, shields: &[(ShieldRequest, u64)]| {
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, _| {
                    swaps.reload_records();
                    swaps.attributions.insert(
                        operation,
                        wallet_ops::ExecutorAttribution::with_shields_for_tests(
                            Address::ZERO,
                            shields,
                        ),
                    );
                    swaps.stage(swaps.record(operation).unwrap())
                })
            })
        };
        executors
            .record_swap_observations(operation, uid, handed_off(None, None))
            .unwrap();
        // The surplus is recovered while the deposit is bridging.
        let mut shields = vec![recover(3, 40)];
        assert_eq!(
            stage(cx, &shields),
            SwapStage::Order(SwapOrderState::Bridging)
        );
        let refunding = Some(SwapBridgeOutcome::Refunding);
        executors
            .record_swap_observations(operation, uid, handed_off(refunding, None))
            .unwrap();
        assert_eq!(
            stage(cx, &shields),
            SwapStage::Order(SwapOrderState::Refunding)
        );
        // The refund arrives after that recovery.
        let refund = SwapObservation {
            block: at(41),
            transaction_hash: Some(B256::repeat_byte(0x90)),
        };
        executors
            .record_swap_observations(operation, uid, handed_off(refunding, Some(refund)))
            .unwrap();
        assert_eq!(
            stage(cx, &shields),
            SwapStage::Order(SwapOrderState::Refunding)
        );
        shields.push(recover(4, 55));
        assert_eq!(stage(cx, &shields), SwapStage::Recovered);
    });
}

/// Check status on an Across refund asks Across about the deposit again, on the swap's own
/// route, since the deposit may have been filled after all. The refund is on this network, so
/// a failed provider check still leaves the balance check to run.
#[gpui::test]
fn checking_an_across_refund_asks_across_before_the_balance(cx: &mut TestAppContext) {
    use wallet_ops::vault::{BridgeDelivery, BridgeProvider, BridgeSurplus, SwapBridgeOutcome};

    let stubs = SwapStubs::start();
    with_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver: Address::repeat_byte(0x51),
            destination_token: STUB_POLYGON_USDC,
            surplus: BridgeSurplus::Reshield,
            private: None,
        });
        let (uid, observed) = placed_swap(executors, operation, delivery);
        executors
            .record_swap_observations(
                operation,
                uid,
                bridge_observations(
                    observed,
                    Some(U256::from(7)),
                    Some(SwapBridgeOutcome::Refunding),
                ),
            )
            .unwrap();
        let orderbook = stub_orderbook(&stubs, runtime);
        cx.update(|window, cx| {
            root.update(cx, |root, _| enable_stub_chain(root, &stubs, 137));
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.bridge_clients = Some(stub_bridge_clients(&stubs, &orderbook));
                tracking.orderbook = Some(orderbook);
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let check = cx.debug_bounds("swap-progress-check").unwrap();
        cx.simulate_click(check.center(), gpui::Modifiers::none());
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| swaps.job.is_none())
        });
        assert!(
            stubs
                .bridge_requests()
                .iter()
                .any(|path| path.starts_with("/across/deposit"))
        );
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| {
                let record = swaps.record(operation).unwrap();
                assert_eq!(
                    swaps.stage(record),
                    SwapStage::Order(SwapOrderState::Refunding)
                );
                // The stub's deposit record is invalid, and the unreachable RPC then fails the
                // balance check, whose error is the one shown.
                let error = swaps.tracking[&operation].error.clone().unwrap();
                assert!(!error.contains("Across"), "{error}");
            });
        });
    });
}

/// Check jobs can persist an outcome before a later RPC fails. With the destination still
/// loaded, both that failure and a successful correction must update its outcome. Neither
/// outcome revokes a shield signature at the unchanged nonce. The checked swap reuses both
/// accounts of an earlier, delivered swap, and each swap's delivery stays its own.
#[gpui::test]
fn manual_checks_reconcile_destination_payloads_even_after_a_later_error(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::ExecutorAsset;
    use wallet_ops::vault::{
        AcrossOrderTerms, BridgeOrderTerms, ExecutorInputIdentity, ExecutorNonceObservation,
        ExecutorPayloadContext, ExecutorPayloadPurpose, IssuedExecutorPayload, SwapAccountChoice,
        SwapAttempt, SwapBridgeOutcome, SwapDestinationClaim, SwapDestinationOutcome,
        SwapPairClaim, SwapProof, SwapTerms, SwapUseId,
    };

    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, origin, _, runtime, store, cx| {
            cx.update(|_, cx| root.update(cx, |root, _| enable_stub_chain(root, &stubs, 137)));
            let session = start_polygon_session(root, &stubs, runtime, store, cx);
            // Each account is delegated as its own network accepts, so both can be reused.
            let (db, view, delegate, origin_delegate) = root.read_with(cx, |root, _| {
                let delegate = |chain_id| {
                    root.effective_chain_configs
                        .get(chain_id)
                        .unwrap()
                        .accepted_executor_profile()
                        .unwrap()
                        .delegate()
                };
                (
                    root.vault_store.as_ref().unwrap().db(),
                    root.view_session.clone().unwrap(),
                    delegate(137),
                    delegate(1),
                )
            });
            let destination = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
            let PlacedPrivateBridge {
                operation,
                destination_operation,
                delivery: initial_delivery,
                uid: earlier_uid,
                origin_observed,
                ..
            } = placed_private_bridge(
                origin,
                &destination,
                origin_delegate,
                delegate,
                STUB_POLYGON_USDT,
            );
            let shield_at = |nonce: u64, block: u64, hash: u8| {
                let observed = ExecutorNonceObservation::new(
                    BlockNumHash::new(block, B256::repeat_byte(30)),
                    U256::from(nonce),
                );
                confirm_setup(&destination, destination_operation, observed);
                IssuedExecutorPayload::new(
                    U256::from(nonce),
                    delegate,
                    B256::repeat_byte(hash),
                    ExecutorPayloadPurpose::SwapDestinationShield,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"shield"),
                        observed,
                        Vec::new(),
                    ),
                )
            };
            // The earlier swap delivered USDT: its fill ran the first shield.
            let earlier = SwapUseId::first(operation);
            let earlier_delivery = SwapBridgeOutcome::DeliveredVerified {
                block: BlockNumHash::new(25, B256::repeat_byte(25)),
                transaction_hash: B256::repeat_byte(26),
                output_amount: BRIDGE_MINIMUM,
                shielded: true,
            };
            let earlier_outcome = Some(SwapDestinationOutcome::Shielded {
                block: BlockNumHash::new(25, B256::repeat_byte(25)),
                transaction_hash: B256::repeat_byte(26),
            });
            origin
                .record_swap_observations(
                    operation,
                    earlier_uid,
                    bridge_observations(origin_observed, Some(U256::from(7)), None),
                )
                .unwrap();
            let placed = origin
                .record_swap_bridge_outcome(operation, earlier_uid, earlier_delivery)
                .unwrap();
            assert!(destination.reconcile_swap_destinations().unwrap());

            // Both settled accounts take a second swap, receiving USDC this time, which signs
            // its own shield and order at the accounts' current nonces.
            let delivery = SwapDelivery::Bridge(BridgeDelivery {
                destination_token: STUB_POLYGON_USDC,
                ..initial_delivery
            });
            let swap_use = SwapUseId::random().unwrap();
            let origin_setup = &placed.issued()[0];
            let origin_observed = ExecutorNonceObservation::new(
                BlockNumHash::new(38, B256::repeat_byte(38)),
                U256::from(3),
            );
            origin
                .record_account_read(operation, origin_observed)
                .unwrap();
            let shield = shield_at(2, 38, 71);
            origin
                .claim_swap_pair(SwapPairClaim {
                    id: swap_use,
                    source: SwapAccountChoice::Existing(operation),
                    delegate: origin_delegate,
                    purpose_summary: None,
                    assets: Vec::new(),
                    approval: SwapApproval {
                        delivery,
                        ..test_approval()
                    },
                    destination: Some(SwapDestinationClaim {
                        chain_id: 137,
                        account: SwapAccountChoice::Existing(destination_operation),
                        delegate,
                        destination_token: STUB_POLYGON_USDC,
                    }),
                })
                .unwrap();
            destination
                .record_swap_destination_shield(destination_operation, swap_use, shield.clone())
                .unwrap();
            let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
                "tree": 4, "position": 16199, "commitment": "0x3"
            }))
            .unwrap();
            let hook = |nonce: u64, hash, purpose, inputs| {
                IssuedExecutorPayload::new(
                    U256::from(nonce),
                    origin_delegate,
                    B256::repeat_byte(hash),
                    purpose,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"new hook"),
                        origin_observed,
                        inputs,
                    ),
                )
            };
            let uid = OrderUid::new(B256::repeat_byte(0x12), Address::repeat_byte(3), u32::MAX);
            let terms = placed.swap().unwrap().terms();
            let mut bounds = test_approval().bounds;
            bounds.destination_minimum = Some(BRIDGE_MINIMUM);
            origin
                .record_swap_attempt(
                    operation,
                    SwapAttempt {
                        use_id: swap_use,
                        terms: SwapTerms::new(
                            terms.sell_token(),
                            terms.buy_token(),
                            terms.recipient(),
                            origin_setup.hash(),
                        ),
                        proof: SwapProof::new(B256::repeat_byte(9), vec![input.clone()]),
                        uid,
                        submission: None,
                        delivery,
                        invalidates: None,
                        pre_hook: hook(3, 9, ExecutorPayloadPurpose::SwapPreHook, vec![input]),
                        post_hook: Some(hook(
                            4,
                            10,
                            ExecutorPayloadPurpose::SwapPostHook,
                            Vec::new(),
                        )),
                        bridge: Some(BridgeOrderTerms::Across(AcrossOrderTerms {
                            spoke_pool: Address::repeat_byte(0x55),
                            input_token: terms.buy_token(),
                            output_token: STUB_POLYGON_USDC,
                            input_amount: bounds.buy_amount,
                            output_amount: BRIDGE_MINIMUM,
                            quote_timestamp: 1_790_000_000,
                            fill_deadline: 1_790_007_200,
                            exclusive_relayer: Address::ZERO,
                            exclusivity_parameter: 0,
                            recipient: Some(Address::repeat_byte(0x56)),
                            message_hash: Some(B256::repeat_byte(0x57)),
                        })),
                        bounds,
                    },
                )
                .unwrap();
            origin
                .record_swap_observations(
                    operation,
                    uid,
                    bridge_observations(origin_observed, Some(U256::from(8)), None),
                )
                .unwrap();
            // What each swap's use of the destination account records of its delivery.
            let outcomes = || {
                let record = destination.records().unwrap().remove(0);
                assert_eq!(record.active_swap_use(), Some(swap_use));
                (
                    record.swap_destination_use(earlier).unwrap().outcome,
                    record.swap_destination_use(swap_use).unwrap().outcome,
                )
            };

            // A status check that reports the earlier swap's fill again settles only that
            // swap. The second swap still waits for its own delivery, and My orders keeps the
            // delivered swap apart from it.
            let writer = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.start_job(
                        operation,
                        SwapJobKind::Check,
                        async move {
                            writer.record_swap_bridge_outcome(
                                operation,
                                earlier_uid,
                                earlier_delivery,
                            )?;
                            eyre::Ok(())
                        },
                        |_, (), _, _| {},
                        window,
                        cx,
                    );
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.job.is_none())
            });
            assert!(!destination.reconcile_swap_destinations().unwrap());
            assert_eq!(outcomes(), (earlier_outcome, None));
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.reload_destinations(cx);
                    let record = swaps.record(operation).unwrap();
                    assert_ne!(
                        swaps.progress_stage(record),
                        SwapStage::Order(SwapOrderState::Done)
                    );
                    let past = SwapIdentity {
                        operation,
                        swap_use: earlier,
                    };
                    let (_, _, order) = swaps.past_swap(past, 0).unwrap();
                    assert_eq!(order.uid(), earlier_uid);
                    assert_eq!(
                        model::swap_order_stage(record, order, swaps.attribution(record)),
                        SwapStage::Order(SwapOrderState::Done)
                    );
                    // Each swap resolves the destination account its own use names.
                    let private = swap_private_delivery(record).unwrap();
                    for swap in [past, swaps.shown_swap(record).unwrap()] {
                        assert_eq!(
                            swaps
                                .swap_destination_account(swap, private)
                                .map(ExecutorRecord::operation),
                            Some(destination_operation)
                        );
                    }
                });
            });
            let block = BlockNumHash::new(40, B256::repeat_byte(40));
            let transaction_hash = B256::repeat_byte(41);
            for (outcome, expected, fail_after) in [
                (
                    SwapBridgeOutcome::Refunding,
                    SwapDestinationOutcome::Unfilled,
                    true,
                ),
                (
                    SwapBridgeOutcome::HeldOnDestination {
                        block,
                        transaction_hash,
                        amount: BRIDGE_MINIMUM,
                    },
                    SwapDestinationOutcome::Held {
                        block,
                        transaction_hash,
                    },
                    false,
                ),
            ] {
                let writer = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.reload_records();
                        swaps.start_job(
                            operation,
                            SwapJobKind::Check,
                            async move {
                                writer.record_swap_bridge_outcome(operation, uid, outcome)?;
                                if fail_after {
                                    Err(eyre::eyre!(
                                        "balance lookup failed after recording outcome"
                                    ))
                                } else {
                                    Ok(())
                                }
                            },
                            |_, (), _, _| {},
                            window,
                            cx,
                        );
                    });
                });
                drive_until(cx, runtime, |cx| {
                    swaps.read_with(cx, |swaps, _| swaps.job.is_none())
                        && destination.records().unwrap()[0]
                            .swap_destination()
                            .unwrap()
                            .outcome
                            == Some(expected)
                });
                // The earlier swap's delivery stays as it was recorded.
                assert_eq!(outcomes(), (earlier_outcome, Some(expected)));
                let record = destination.records().unwrap().remove(0);
                assert_eq!(
                    (
                        record.is_outstanding_at(&shield, U256::from(2)),
                        record.has_competing_payloads(),
                        record.has_unresolved_issued_work()
                    ),
                    (true, true, true)
                );
            }
            // Reload without an originating swap view supplying its receiving token. Direct
            // account balance checks and recovery discover their assets from this record.
            let reloaded = ExecutorStore::new(db, view, 137).unwrap();
            let record = reloaded.records().unwrap().remove(0);
            assert!(
                record
                    .assets()
                    .contains(&ExecutorAsset::Erc20(STUB_POLYGON_USDT))
            );
            assert_eq!(
                record
                    .assets()
                    .iter()
                    .filter(|asset| **asset == ExecutorAsset::Erc20(STUB_POLYGON_USDC))
                    .count(),
                1,
                "the later receiving token must be persisted for direct account recovery"
            );
            assert_eq!(
                record
                    .swap_destination_use(earlier)
                    .unwrap()
                    .destination_token,
                STUB_POLYGON_USDT
            );
            assert_eq!(
                record
                    .swap_destination_use(swap_use)
                    .unwrap()
                    .destination_token,
                STUB_POLYGON_USDC
            );
            runtime.block_on(session.stop()).unwrap();
        },
    );
}

/// A NEAR Intents deposit that needs attention has its own My orders group, apart from
/// recovery, and shows its deposit address in full. Check status asks 1Click again on the
/// swap's own route and records the delivery it reports.
#[gpui::test]
fn near_intents_deposit_that_needs_attention_is_checked_with_the_provider(cx: &mut TestAppContext) {
    use wallet_ops::vault::{BridgeDelivery, BridgeProvider, BridgeSurplus, SwapBridgeOutcome};

    let stubs = SwapStubs::start();
    with_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::NearIntents,
            destination_chain: 137,
            receiver: Address::repeat_byte(0x51),
            destination_token: Address::ZERO,
            surplus: BridgeSurplus::BridgedByProvider,
            private: None,
        });
        let (uid, observed) = placed_swap(executors, operation, delivery);
        executors
            .record_swap_observations(
                operation,
                uid,
                bridge_observations(observed, None, Some(SwapBridgeOutcome::NeedsAttention)),
            )
            .unwrap();
        let orderbook = stub_orderbook(&stubs, runtime);
        cx.update(|_, cx| {
            root.update(cx, |root, _| enable_stub_chain(root, &stubs, 137));
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                let stage = swaps.stage(record);
                assert_eq!(stage, SwapStage::Order(SwapOrderState::NeedsAttention));
                assert!(model::swap_card_line(stage, &swaps.labels(record, cx)).attention);
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.bridge_clients = Some(stub_bridge_clients(&stubs, &orderbook));
                tracking.orderbook = Some(orderbook);
            });
        });
        // The wallet can't recover the deposit, so it isn't listed as needing recovery.
        let row: &'static str = format!("swap-order-row-{}", operation.opaque_id()).leak();
        for (filter, listed) in [
            (model::SwapOrderGroup::NeedsAttention, true),
            (model::SwapOrderGroup::NeedsRecovery, false),
        ] {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.orders_filter = Some(filter);
                    swaps.show_view(dialog::SwapDialogView::Orders, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            assert_eq!(cx.debug_bounds(row).is_some(), listed, "{filter:?}");
        }
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| swaps.show_detail(operation, window, cx));
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-detail-deposit-address").is_some());
        assert!(cx.debug_bounds("swap-progress-recover").is_none());
        let check = cx.debug_bounds("swap-progress-check").unwrap();
        cx.simulate_click(check.center(), gpui::Modifiers::none());
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| swaps.job.is_none())
        });
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                let record = swaps.record(operation).unwrap();
                let stage = swaps.stage(record);
                assert_eq!(
                    stage,
                    SwapStage::Order(SwapOrderState::Done),
                    "{:?}",
                    swaps.tracking[&operation].error
                );
                let labels = swaps.labels(record, cx);
                assert_eq!(
                    model::swap_steps(stage, &labels).last().unwrap().label,
                    "Delivered on Polygon · reported by NEAR Intents"
                );
                assert_eq!(
                    labels.received,
                    Some(swaps.network_token_amount(
                        137,
                        Address::ZERO,
                        STUB_NEAR_EXPECTED.parse().unwrap(),
                        cx
                    ))
                );
            });
        });
        assert!(
            stubs
                .bridge_requests()
                .iter()
                .any(|path| path.starts_with("/near/v0/status"))
        );
    });
}

/// A filled order's outcome shows what it received against its minimum. Its fee row waits for
/// the fee the orderbook charged, which the swap reads once per session on its own route, also
/// after verification ends routine polling: a failed read isn't repeated until the next session.
#[gpui::test]
fn a_filled_order_reads_its_executed_fee_once_per_session(cx: &mut TestAppContext) {
    use wallet_ops::vault::{SwapObservation, SwapOrderObservations, SwapTradeAmounts};

    let stubs = SwapStubs::start();
    with_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        let delivery = SwapDelivery::External {
            receiver: Address::repeat_byte(0x51),
        };
        let (uid, observed) = placed_swap_with(executors, operation, delivery, |bounds| {
            bounds.gas_share_bps = Some(2_500);
        });
        let seen = SwapObservation {
            block: observed.block(),
            transaction_hash: Some(alloy::primitives::B256::repeat_byte(40)),
        };
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
                        settlement_gas_used: Some(250_000),
                        settlement_effective_gas_price: Some(2_000_000_000),
                        executed_fee: None,
                        executed_fee_token: None,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        // One pass of the orderbook poller, or `false` when it has nothing to ask.
        let poll = |cx: &mut gpui::VisualTestContext| {
            let hints = cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.next_order_hints(cx)
                })
            });
            let Some((_, owner, requests)) = hints else {
                return false;
            };
            assert_eq!(requests.len(), 1);
            assert!(
                requests[0].fee && !requests[0].hint,
                "a verified order asks only for its fee"
            );
            let results = runtime.block_on(fetch_order_hints(owner, requests));
            cx.update(|_, cx| swaps.update(cx, |swaps, cx| swaps.apply_order_hints(results, cx)));
            true
        };
        let executed_fee = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| swaps.update(cx, |swaps, _| swaps.reload_records()));
            swaps.read_with(cx, |swaps, _| {
                let order = swaps.record(operation)?.swap()?.orders().last()?;
                order
                    .observations()
                    .trade_amounts?
                    .executed_fee
                    .zip(order.observations().trade_amounts?.executed_fee_token)
            })
        };
        let show = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| swaps.show_detail(operation, window, cx));
                window.draw(cx).clear(cx);
            });
        };

        // The orderbook is unreachable: nothing is recorded, and nothing is asked again.
        let unreachable = orderbook_at("http://127.0.0.1:1/mainnet".parse().unwrap(), runtime);
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| {
                swaps.tracking.entry(operation).or_default().orderbook = Some(unreachable);
            });
        });
        assert!(poll(cx));
        assert_eq!(executed_fee(cx), None);
        assert!(
            !poll(cx),
            "a failed fee read isn't repeated in the same session"
        );
        show(cx);
        assert!(cx.debug_bounds("swap-outcome-received").is_some());
        assert!(cx.debug_bounds("swap-outcome-minimum").is_some());
        assert!(
            cx.debug_bounds("swap-outcome-gas").is_none(),
            "no fee row without the charged fee"
        );

        // The next session asks once on the swap's route, and the fee persists.
        let orderbook = stub_orderbook(&stubs, runtime);
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| {
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.fee_asked.clear();
                tracking.orderbook = Some(orderbook);
            });
        });
        assert!(poll(cx));
        let requests = stubs.order_requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].ends_with(&format!("/api/v1/orders/{}", uid.0)));
        assert_eq!(
            executed_fee(cx),
            Some((U256::from(STUB_EXECUTED_FEE), Address::repeat_byte(2)))
        );
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| swaps.tracking.clear());
        });
        assert!(!poll(cx), "a recorded fee is never asked for again");
        assert_eq!(stubs.order_requests().len(), 1);
        show(cx);
        assert!(cx.debug_bounds("swap-outcome-gas").is_some());
    });
}

/// An order that expired unfilled is a Not filled warning with its terms and where the funds
/// are. Swap again reopens the form with the share, tolerance and validity it was signed with.
#[gpui::test]
fn an_unfilled_expiry_shows_its_terms_and_swaps_again_with_them(cx: &mut TestAppContext) {
    unfilled_expiry(cx, false);
}

/// A record from before gas shares expires without share-specific rows, and Swap again keeps
/// its tolerance but falls back to Balanced and the profile's validity.
#[gpui::test]
fn a_legacy_unfilled_expiry_swaps_again_with_balanced(cx: &mut TestAppContext) {
    unfilled_expiry(cx, true);
}

fn unfilled_expiry(cx: &mut TestAppContext, legacy: bool) {
    use crate::root::public_action::PublicActionStepStatus;
    use wallet_ops::vault::{
        SwapObservation, SwapOrderObservations, SwapPreHookDeath, SwapPreHookDeathCause,
    };

    with_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        let (uid, observed) =
            placed_swap_with(executors, operation, SwapDelivery::Reshield, |bounds| {
                bounds.slippage_bps = 120;
                if !legacy {
                    bounds.gas_share_bps = Some(1_000);
                    bounds.gas_estimate = Some(U256::from(40));
                    bounds.gas_allowance = Some(U256::from(4));
                    bounds.gas_price_wei = Some(1_180_000_000);
                    bounds.valid_for_secs = Some(1_800);
                }
            });
        executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    pre_hook_dead: Some(SwapPreHookDeath {
                        cause: SwapPreHookDeathCause::Expired,
                        observation: SwapObservation {
                            block: observed.block(),
                            transaction_hash: None,
                        },
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        // A quote the reopened form schedules goes nowhere.
        let unreachable = orderbook_at("http://127.0.0.1:1/mainnet".parse().unwrap(), runtime);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().orderbook = Some(unreachable);
                let record = swaps.record(operation).unwrap();
                let stage = swaps.stage(record);
                assert_eq!(
                    stage,
                    SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired))
                );
                let steps = model::swap_steps(stage, &swaps.labels(record, cx));
                let ended = steps.last().unwrap();
                assert_eq!(
                    (ended.label.as_str(), ended.status),
                    ("Not filled", PublicActionStepStatus::Warning)
                );
                let covered = if legacy {
                    "No solver filled it before the order expired at "
                } else {
                    "No solver covered "
                };
                assert!(ended.detail.starts_with(covered), "{}", ended.detail);
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-not-filled").is_some());
        assert!(cx.debug_bounds("swap-outcome-minimum").is_some());
        for selector in ["swap-not-filled-gas", "swap-not-filled-gas-price"] {
            assert_eq!(
                cx.debug_bounds(selector).is_some(),
                !legacy,
                "{selector} for a legacy record: {legacy}"
            );
        }

        let retry = cx.debug_bounds("swap-progress-retry").unwrap();
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        cx.update(|_, cx| {
            let swaps = swaps.read(cx);
            let form = swaps.form.as_ref().expect("Swap again opens the form");
            assert_eq!(form.slippage_bps, 120);
            let expected = if legacy {
                (GAS_SHARE_BALANCED_BPS, swaps.default_valid_for(cx))
            } else {
                (1_000, Duration::from_mins(30))
            };
            assert_eq!((form.gas_share_bps, form.valid_for), expected);
        });
    });
}

const STUB_USDC: Address = alloy::primitives::address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
/// What the stub orderbook quotes for any sell amount: 0.0004 ETH.
const STUB_BUY_AMOUNT: u64 = 400_000_000_000_000;
/// The fee the stub orderbook reports it charged any order, in `Address::repeat_byte(2)`.
const STUB_EXECUTED_FEE: u64 = 464_572;

/// Local stand-ins for the chain RPC, the `CoW` orderbook and the bridge providers, served from
/// their own thread, so a quote goes through the real planning and review without live
/// services. The RPC answers `eth_gasPrice` and fails everything else, like the unreachable RPC
/// of other tests, with a gas price of 1 wei unless set. The orderbook quotes
/// [`STUB_BUY_AMOUNT`] for any order, with no network fee unless set, and keeps each request's
/// body, and reports every order
/// fulfilled with [`STUB_EXECUTED_FEE`], keeping each report's path. Across and 1Click list the routes of [`stub_bridge_list`] and quote as
/// [`stub_bridge_reply`] describes.
struct SwapStubs {
    url: reqwest::Url,
    quotes: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    bridge_requests: Arc<std::sync::Mutex<Vec<String>>>,
    order_requests: Arc<std::sync::Mutex<Vec<String>>>,
    /// The path of each RPC request, which tells apart chains given their own path.
    rpc_requests: Arc<std::sync::Mutex<Vec<String>>>,
    across_fee_bps: Arc<std::sync::atomic::AtomicU64>,
    across_relayer_gas_bps: Arc<std::sync::atomic::AtomicU64>,
    across_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    failing: Arc<std::sync::Mutex<Vec<&'static str>>>,
    gas_price_wei: Arc<std::sync::atomic::AtomicU64>,
    public_review_rpc: Arc<std::sync::atomic::AtomicBool>,
    public_allowance_complete: Arc<std::sync::atomic::AtomicBool>,
    math_deployed: Arc<std::sync::atomic::AtomicBool>,
    fee_amount: Arc<std::sync::atomic::AtomicU64>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// What the stub providers quote with: Across deposits into the swap chain's `spoke_pool` and
/// keeps `across_fee_bps` of the amount, after `across_delay_ms`. Requests to a path starting
/// with one of `failing` get a 503.
#[derive(Clone)]
struct StubBridge {
    spoke_pool: Address,
    across_fee_bps: Arc<std::sync::atomic::AtomicU64>,
    across_relayer_gas_bps: Arc<std::sync::atomic::AtomicU64>,
    across_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    failing: Arc<std::sync::Mutex<Vec<&'static str>>>,
    public_review_rpc: Arc<std::sync::atomic::AtomicBool>,
    public_allowance_complete: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the math contract an order's post-hook calls has its code on the stub chain.
    math_deployed: Arc<std::sync::atomic::AtomicBool>,
}

impl SwapStubs {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let quotes = Arc::<std::sync::Mutex<Vec<serde_json::Value>>>::default();
        let bridge_requests = Arc::<std::sync::Mutex<Vec<String>>>::default();
        let order_requests = Arc::<std::sync::Mutex<Vec<String>>>::default();
        let rpc_requests = Arc::<std::sync::Mutex<Vec<String>>>::default();
        let across_fee_bps = Arc::new(std::sync::atomic::AtomicU64::new(10));
        let across_relayer_gas_bps = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let across_delay_ms = Arc::<std::sync::atomic::AtomicU64>::default();
        let failing = Arc::<std::sync::Mutex<Vec<&'static str>>>::default();
        let gas_price_wei = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let public_review_rpc = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let public_allowance_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let math_deployed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let served_gas_price = Arc::clone(&gas_price_wei);
        let fee_amount = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let served_fee_amount = Arc::clone(&fee_amount);
        let recorded = Arc::clone(&quotes);
        let recorded_bridge = Arc::clone(&bridge_requests);
        let recorded_orders = Arc::clone(&order_requests);
        let recorded_rpc = Arc::clone(&rpc_requests);
        let bridge = StubBridge {
            spoke_pool: wallet_ops::settings::build_effective_chain_configs(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap()
            .get(1)
            .unwrap()
            .bridge_profile()
            .unwrap()
            .spoke_pool(),
            across_fee_bps: Arc::clone(&across_fee_bps),
            across_relayer_gas_bps: Arc::clone(&across_relayer_gas_bps),
            across_delay_ms: Arc::clone(&across_delay_ms),
            failing: Arc::clone(&failing),
            public_review_rpc: Arc::clone(&public_review_rpc),
            public_allowance_complete: Arc::clone(&public_allowance_complete),
            math_deployed: Arc::clone(&math_deployed),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let serve = async {
                    while let Ok((stream, _)) = listener.accept().await {
                        tokio::spawn(stub_response(
                            stream,
                            Arc::clone(&recorded),
                            Arc::clone(&recorded_bridge),
                            Arc::clone(&recorded_orders),
                            Arc::clone(&recorded_rpc),
                            bridge.clone(),
                            Arc::clone(&served_gas_price),
                            Arc::clone(&served_fee_amount),
                        ));
                    }
                };
                tokio::select! {
                    () = serve => {}
                    _ = stopped => {}
                }
            });
        });
        Self {
            url,
            quotes,
            bridge_requests,
            order_requests,
            rpc_requests,
            across_fee_bps,
            across_relayer_gas_bps,
            across_delay_ms,
            failing,
            gas_price_wei,
            public_review_rpc,
            public_allowance_complete,
            math_deployed,
            fee_amount,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Have Across keep `bps` of the amount from now on.
    fn set_across_fee_bps(&self, bps: u64) {
        self.across_fee_bps
            .store(bps, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have Across hold each fee quote it is asked for from now on for `delay`.
    fn set_across_delay(&self, delay: Duration) {
        self.across_delay_ms.store(
            u64::try_from(delay.as_millis()).unwrap(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Have the providers answer 503 to paths starting with one of `prefixes` from now on.
    fn set_failing(&self, prefixes: &[&'static str]) {
        *self.failing.lock().unwrap() = prefixes.to_vec();
    }

    /// Have the RPC report `wei` as its gas price from now on.
    fn set_gas_price_wei(&self, wei: u64) {
        self.gas_price_wei
            .store(wei, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have the orderbook quote `fee` of the sell amount as its network fee from now on.
    fn set_fee_amount(&self, fee: u64) {
        self.fee_amount
            .store(fee, std::sync::atomic::Ordering::Relaxed);
    }

    fn rpc(&self) -> reqwest::Url {
        self.url.join("rpc").unwrap()
    }

    /// Public previews read an existing allowance, whether the `CoW` proxy is deployed, and
    /// the math contract's code.
    fn enable_public_reviews(&self) {
        self.public_review_rpc
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have the chain answer a read of the math contract's code with its runtime code, as it
    /// does at first, or with none, from now on.
    fn set_math_deployed(&self, deployed: bool) {
        self.math_deployed
            .store(deployed, std::sync::atomic::Ordering::Relaxed);
    }

    fn orderbook(&self) -> reqwest::Url {
        self.url.join("mainnet").unwrap()
    }

    /// The bodies of the quote requests so far.
    fn quotes(&self) -> Vec<serde_json::Value> {
        self.quotes.lock().unwrap().clone()
    }

    /// The paths of the bridge provider requests so far.
    fn bridge_requests(&self) -> Vec<String> {
        self.bridge_requests.lock().unwrap().clone()
    }

    /// The paths of the orderbook's order reports so far.
    fn order_requests(&self) -> Vec<String> {
        self.order_requests.lock().unwrap().clone()
    }

    /// How many RPC requests arrived on `path` so far.
    fn rpc_requests_on(&self, path: &str) -> usize {
        let requests = self.rpc_requests.lock().unwrap();
        requests.iter().filter(|request| *request == path).count()
    }
}

impl Drop for SwapStubs {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answer one request, then close the connection.
async fn stub_response(
    stream: tokio::net::TcpStream,
    quotes: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    bridge_requests: Arc<std::sync::Mutex<Vec<String>>>,
    order_requests: Arc<std::sync::Mutex<Vec<String>>>,
    rpc_requests: Arc<std::sync::Mutex<Vec<String>>>,
    bridge: StubBridge,
    gas_price_wei: Arc<std::sync::atomic::AtomicU64>,
    fee_amount: Arc<std::sync::atomic::AtomicU64>,
) {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::io::BufReader::new(stream);
    let mut request_line = String::new();
    if stream.read_line(&mut request_line).await.unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    if stream.read_exact(&mut body).await.is_err() {
        return;
    }
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let path = request_line.split(' ').nth(1).unwrap_or_default();
    if path.starts_with("/rpc") {
        rpc_requests.lock().unwrap().push(path.to_owned());
    }
    let failing = bridge
        .failing
        .lock()
        .unwrap()
        .iter()
        .any(|prefix| path.starts_with(prefix));
    let status = if failing {
        "503 Service Unavailable"
    } else {
        "200 OK"
    };
    let reply = if path.starts_with("/across/") || path.starts_with("/near/") {
        bridge_requests.lock().unwrap().push(path.to_owned());
        if path.starts_with("/across/suggested-fees") {
            let delay = bridge
                .across_delay_ms
                .load(std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        stub_bridge_reply(path, &body, &bridge)
    } else if request_line.contains("/api/v1/quote") {
        // Price the sell token so that one buy-token base unit is worth one wei, which keeps
        // the hook allowance far below the quoted output for any sell amount.
        #[allow(clippy::cast_precision_loss)]
        let sell_token_price = body["sellAmountBeforeFee"]
            .as_str()
            .and_then(|amount| amount.parse::<f64>().ok())
            .map_or_else(
                || "1".to_owned(),
                |amount| (STUB_BUY_AMOUNT as f64 / amount).to_string(),
            );
        // The network fee comes out of the sell amount, as `CoW` quotes a sell order.
        let fee = U256::from(fee_amount.load(std::sync::atomic::Ordering::Relaxed));
        let sell_amount = body["sellAmountBeforeFee"]
            .as_str()
            .and_then(|amount| amount.parse::<U256>().ok())
            .map_or_else(
                || body["sellAmountBeforeFee"].clone(),
                |amount| amount.saturating_sub(fee).to_string().into(),
            );
        let reply = serde_json::json!({
            "quote": {
                "sellToken": body["sellToken"], "buyToken": body["buyToken"],
                "sellAmount": sell_amount,
                "buyAmount": STUB_BUY_AMOUNT.to_string(), "validTo": 1,
                "feeAmount": fee.to_string(),
                "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": sell_token_price, "kind": "sell",
                "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        });
        quotes.lock().unwrap().push(body);
        reply
    } else if path.contains("/api/v1/orders/") {
        order_requests.lock().unwrap().push(path.to_owned());
        serde_json::json!({
            "status": "fulfilled",
            "executedFee": STUB_EXECUTED_FEE.to_string(),
            "executedFeeToken": Address::repeat_byte(2),
        })
    } else if bridge
        .public_review_rpc
        .load(std::sync::atomic::Ordering::Relaxed)
        && body["method"] == "eth_call"
    {
        let allowance = body["params"][0]["input"]
            .as_str()
            .or_else(|| body["params"][0]["data"].as_str())
            .is_some_and(|input| input.starts_with("0xdd62ed3e"));
        let amount = if allowance
            && bridge
                .public_allowance_complete
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            U256::MAX
        } else {
            U256::ZERO
        };
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
            "result": format!("0x{}", alloy::hex::encode(amount.to_be_bytes::<32>()))})
    } else if bridge
        .public_review_rpc
        .load(std::sync::atomic::Ordering::Relaxed)
        && body["method"] == "eth_getCode"
    {
        // Only the math contract has code: the account's `CoW` proxy isn't deployed.
        let math = body["params"][0]
            .as_str()
            .and_then(|address| address.parse::<Address>().ok())
            == Some(SWAP_MATH_ADDRESS)
            && bridge
                .math_deployed
                .load(std::sync::atomic::Ordering::Relaxed);
        let code = if math {
            &SWAP_MATH_CREATION_CODE[30..]
        } else {
            &[]
        };
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
            "result": alloy::hex::encode_prefixed(code)})
    } else if body["method"] == "eth_gasPrice" {
        let price = gas_price_wei.load(std::sync::atomic::Ordering::Relaxed);
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": format!("{price:#x}")})
    } else {
        serde_json::json!({
            "jsonrpc": "2.0", "id": body["id"],
            "error": {"code": -32601, "message": "unavailable in tests"}
        })
    };
    let reply = reply.to_string();
    let _ = stream
        .get_mut()
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .as_bytes(),
        )
        .await;
}

/// Mainnet USDT and Polygon's USDC and USDT, all in the default token list.
const STUB_USDT: Address = alloy::primitives::address!("dac17f958d2ee523a2206206994597c13d831ec7");
const STUB_POLYGON_USDC: Address =
    alloy::primitives::address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359");
const STUB_POLYGON_USDT: Address =
    alloy::primitives::address!("c2132d05d31c914a87c6611c10748aeb04b58e8f");

/// WETH on Ethereum, and on Arbitrum One and Base, where it is the wrapped native token.
const STUB_WETH: Address = alloy::primitives::address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
const STUB_ARBITRUM_WETH: Address =
    alloy::primitives::address!("82af49447d8a07e3bd95bd0d56f35241523fbab1");
const STUB_BASE_WETH: Address =
    alloy::primitives::address!("4200000000000000000000000000000000000006");

/// The stub providers' lists from Ethereum. Across bridges USDC and USDT to Polygon, and WETH
/// to Arbitrum One and Base. 1Click lists both stablecoins on Ethereum and Polygon, native POL on
/// Polygon, and native ETH on Arbitrum One.
fn stub_bridge_list(path: &str) -> serde_json::Value {
    let route = |chain: u64, origin: Address, destination: Address, symbol: &str| {
        serde_json::json!({
            "originChainId": 1, "originToken": origin, "destinationChainId": chain,
            "destinationToken": destination, "originTokenSymbol": symbol,
            "destinationTokenSymbol": symbol, "isNative": false
        })
    };
    let token = |blockchain: &str, symbol: &str, contract: Option<Address>| {
        serde_json::json!({
            "assetId": format!("nep141:{blockchain}-{symbol}"), "decimals": 6,
            "blockchain": blockchain, "symbol": symbol, "contractAddress": contract
        })
    };
    if path.starts_with("/across/available-routes") {
        serde_json::json!([
            route(137, STUB_USDC, STUB_POLYGON_USDC, "USDC"),
            route(137, STUB_USDT, STUB_POLYGON_USDT, "USDT"),
            route(42161, STUB_WETH, STUB_ARBITRUM_WETH, "WETH"),
            route(8453, STUB_WETH, STUB_BASE_WETH, "WETH"),
        ])
    } else if path.starts_with("/near/v0/tokens") {
        serde_json::json!([
            token("eth", "USDC", Some(STUB_USDC)),
            token("eth", "USDT", Some(STUB_USDT)),
            token("pol", "POL", None),
            token("pol", "USDC", Some(STUB_POLYGON_USDC)),
            token("pol", "USDT", Some(STUB_POLYGON_USDT)),
            token("arb", "ETH", None),
        ])
    } else {
        serde_json::json!({})
    }
}

/// The seed of the key that stands in for 1Click's quote signer.
const STUB_QUOTE_SEED: u8 = 7;
/// What 1Click's stub quotes deliver, in base units of the destination token: about 12.6 and
/// at least 12.5 of an 18-decimal token.
const STUB_NEAR_EXPECTED: &str = "12600000000000000000";
const STUB_NEAR_MINIMUM: &str = "12500000000000000000";

/// A stub provider's answer. Across quotes a fee of the configured share of the amount; 1Click
/// signs a dry quote of [`STUB_NEAR_EXPECTED`] and [`STUB_NEAR_MINIMUM`] for the request it was
/// sent with the stand-in key, and reports every deposit delivered with [`STUB_NEAR_EXPECTED`].
/// Other requests get the lists of [`stub_bridge_list`].
fn stub_bridge_reply(
    path: &str,
    body: &serde_json::Value,
    bridge: &StubBridge,
) -> serde_json::Value {
    if path.starts_with("/across/suggested-fees") {
        let amount = reqwest::Url::parse(&format!("http://stub{path}"))
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "amount")
                    .and_then(|(_, amount)| amount.parse::<U256>().ok())
            })
            .unwrap_or_default();
        let bps = bridge
            .across_fee_bps
            .load(std::sync::atomic::Ordering::Relaxed);
        let fee = amount * U256::from(bps) / U256::from(10_000_u32);
        serde_json::json!({
            "outputAmount": (amount - fee).to_string(),
            "totalRelayFee": {"pct": "0", "total": fee.to_string()},
            "relayerGasFee": {
                "pct": (U256::from(bridge.across_relayer_gas_bps.load(std::sync::atomic::Ordering::Relaxed))
                    * U256::from(100_000_000_000_000_u64)).to_string(),
                "total": "0"
            },
            "lpFee": {"pct": "0", "total": "0"},
            "timestamp": "1790718359", "fillDeadline": "1790725559",
            "exclusiveRelayer": Address::ZERO, "exclusivityDeadline": 0,
            "spokePoolAddress": bridge.spoke_pool,
            "destinationSpokePoolAddress": Address::repeat_byte(0x55),
            "isAmountTooLow": false,
            "limits": {"minDeposit": "1", "maxDeposit": U256::MAX.to_string()},
            "estimatedFillTimeSec": 2
        })
    } else if path.starts_with("/near/v0/quote") {
        let response = serde_json::json!({
            "quote": {
                "amountIn": body["amount"], "minAmountIn": body["amount"],
                "amountOut": STUB_NEAR_EXPECTED, "minAmountOut": STUB_NEAR_MINIMUM,
                "timeEstimate": 20
            },
            "quoteRequest": body,
            "timestamp": "2026-09-30T00:00:00.000Z"
        });
        serde_json::from_str(&wallet_ops::bridge::sign_quote_response_with_stand_in(
            response,
            STUB_QUOTE_SEED,
        ))
        .unwrap()
    } else if path.starts_with("/near/v0/status") {
        serde_json::json!({
            "status": "SUCCESS",
            "swapDetails": {
                "amountOut": STUB_NEAR_EXPECTED,
                "destinationChainTxHashes": [
                    {"hash": alloy::primitives::B256::repeat_byte(0x5c), "explorerUrl": ""}
                ]
            }
        })
    } else {
        stub_bridge_list(path)
    }
}

/// Bridge clients for the stub providers, on `orderbook`'s route.
fn stub_bridge_clients(stubs: &SwapStubs, orderbook: &CowOrderbookClient) -> SwapBridgeClients {
    SwapBridgeClients {
        across: wallet_ops::bridge::AcrossClient::new(
            orderbook.http().clone(),
            stubs.url.join("across").unwrap(),
        )
        .unwrap(),
        near: wallet_ops::bridge::NearIntentsClient::new(
            orderbook.http().clone(),
            stubs.url.join("near").unwrap(),
            &wallet_ops::bridge::stand_in_quote_key(STUB_QUOTE_SEED),
        )
        .unwrap(),
    }
}

/// A new swap of 1 USDC, planned from a 10 USDC stub note, to `Address::repeat_byte(4)`, with
/// the Buy picker open on Polygon once the stub providers' routes are listed. Picking a token
/// there makes it a Bridge swap. Polygon is enabled with the stub's RPC.
fn open_bridge_form(
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
            swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
            set_receiver_text(swaps, &Address::repeat_byte(4).to_string(), window, cx);
            swaps.open_buy_picker(window, cx);
            swaps.show_buy_picker_network(137, window, cx);
        });
    });
    drive_until(cx, runtime, |cx| {
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            form.bridge.routes.contains_key(&(STUB_USDC, 137))
        })
    });
}

/// Start Polygon in the same wallet scope as the origin fixture and expose its ready session.
fn start_polygon_session(
    root: &Entity<WalletRoot>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    store: &wallet_ops::WalletSessionStore,
    cx: &mut gpui::VisualTestContext,
) -> Arc<WalletSession> {
    let mut lifecycle = WalletSyncLifecycle::new();
    let registration = lifecycle.prepare_startup(137);
    let (request, http) = root.read_with(cx, |root, _| {
        let mut chain = root.effective_chain_configs.get(137).unwrap().clone();
        let private = chain.railgun.as_mut().unwrap();
        private.archive_rpc_url = None;
        private.sync.quick_sync_endpoint = None;
        private.sync.indexed_artifact_source = None;
        let poi = wallet_ops::PoiReadSource::PoiProxy {
            rpc_url: stubs.rpc().into(),
        };
        let request = wallet_ops::ViewWalletChainSessionRequest {
            view_session: root.view_session.clone().unwrap(),
            wallet_scope_generation: registration.generation,
            chain_id: 137,
            effective_chain: chain,
            sync_start_policy: wallet_ops::DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
            init_block_number: Some(0),
            sync_to_block: Some(0),
            use_indexed_wallet_catch_up: false,
            poi_read_source: poi,
            rewind_wallet_cache: false,
            progress_tx: None,
        };
        (request, root.http.clone())
    });
    let session = Arc::new(
        runtime
            .block_on(store.start_view_wallet_session_immediate(request, &http))
            .unwrap(),
    );
    let observation = session.observation_rx.borrow().clone();
    cx.update(|_, cx| {
        root.update(cx, |root, _| {
            root.chain_states.insert(
                137,
                ChainUtxoState::Ready {
                    session: Arc::clone(&session),
                    snapshot: observation.snapshot,
                    observer_token: registration.observer_token,
                    sync_tip: wallet_ops::WalletSyncTip::default(),
                    poi_refreshing: false,
                    ppoi_workflow_status: observation.ppoi_workflow_status,
                },
            );
        });
    });
    session
}

/// The built-in `chain_id` as the default settings configure it.
fn built_in_chain(chain_id: u64) -> EffectiveChainConfig {
    wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(chain_id)
    .unwrap()
    .clone()
}

/// Put `chain` among the wallet's chains, in place of one with its id.
fn put_chain(root: &mut WalletRoot, chain: EffectiveChainConfig) {
    let chain_id = chain.chain_id;
    root.effective_chain_configs = root
        .effective_chain_configs
        .clone()
        .into_values()
        .filter(|known| known.chain_id != chain_id)
        .chain(std::iter::once(chain))
        .collect();
}

/// Enable the built-in `chain_id` with the stub's RPC.
fn enable_stub_chain(root: &mut WalletRoot, stubs: &SwapStubs, chain_id: u64) {
    let mut enabled = built_in_chain(chain_id);
    enabled.enabled = true;
    enabled.rpc_route = wallet_ops::RpcChainRoute::new(chain_id, vec![stubs.rpc()]);
    put_chain(root, enabled);
}

/// An orderbook client for the stub orderbook, on a direct route.
fn stub_orderbook(stubs: &SwapStubs, runtime: &tokio::runtime::Runtime) -> CowOrderbookClient {
    orderbook_at(stubs.orderbook(), runtime)
}

/// An orderbook client for `url`, on a direct route.
fn orderbook_at(url: reqwest::Url, runtime: &tokio::runtime::Runtime) -> CowOrderbookClient {
    let directory = tempfile::tempdir().unwrap();
    runtime.block_on(async {
        let http = wallet_ops::build_wallet_network_context(wallet_ops::WalletNetworkConfig {
            network_mode: Some(wallet_ops::WalletNetworkMode::Direct),
            proxy: None,
            data_dir: directory.path(),
        })
        .await
        .unwrap();
        CowOrderbookClient::new(http.operation_http_client().await.unwrap(), url, 1).unwrap()
    })
}

/// Run the fixture's runtime, where quotes make their requests, and the UI, including the
/// quote debounce, until `done`.
fn drive_until(
    cx: &mut gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
    mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) {
    for _ in 0..500 {
        cx.executor().advance_clock(QUOTE_DEBOUNCE);
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        runtime.block_on(tokio::time::sleep(Duration::from_millis(10)));
    }
    panic!("the swap's requests didn't finish");
}

/// The form's quote, once it's ready.
fn ready_review(
    swaps: &Entity<PrivateSwapsView>,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) -> Arc<SwapReview> {
    drive_until(cx, runtime, |cx| {
        swaps.read_with(cx, |swaps, _| {
            !matches!(
                swaps.form.as_ref().map(|form| &form.quote),
                Some(QuoteState::Loading)
            )
        })
    });
    swaps.read_with(cx, |swaps, _| match &swaps.form.as_ref().unwrap().quote {
        QuoteState::Ready(review) => Arc::clone(review),
        QuoteState::Failed(error) => panic!("{error:#}"),
        _ => panic!("the quote isn't ready"),
    })
}

/// The form's quote once no bridge refresh is under way.
fn refreshed_review(
    swaps: &Entity<PrivateSwapsView>,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) -> Arc<SwapReview> {
    drive_until(cx, runtime, |cx| {
        swaps.read_with(cx, |swaps, _| {
            swaps.form.as_ref().unwrap().quote_task.is_none()
        })
    });
    ready_review(swaps, runtime, cx)
}

/// The amounts of the Across fee quotes asked for so far.
fn across_fee_amounts(stubs: &SwapStubs) -> Vec<U256> {
    stubs
        .bridge_requests()
        .iter()
        .filter(|path| path.starts_with("/across/suggested-fees"))
        .map(|path| {
            reqwest::Url::parse(&format!("http://stub{path}"))
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == "amount")
                .unwrap()
                .1
                .parse()
                .unwrap()
        })
        .collect()
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

/// The swap view after a restart, which knows only the saved records. It keeps the setup's
/// observation, quotes through the stub orderbook and plans from 10 USDC.
fn restarted_swaps(
    root: &Entity<WalletRoot>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    operation: ExecutorOperationId,
    setup: wallet_ops::SwapSetupStatus,
    cx: &mut gpui::VisualTestContext,
) -> Entity<PrivateSwapsView> {
    let orderbook = stub_orderbook(stubs, runtime);
    cx.update(|window, cx| {
        let swaps = root.update(cx, |root, cx| {
            root.effective_token_registry = wallet_ops::settings::build_effective_token_registry(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap();
            root.clear_private_swaps(cx);
            root.ensure_private_swaps(window, cx);
            root.private_swaps_view().unwrap()
        });
        swaps.update(cx, |swaps, _| {
            swaps
                .private_owner()
                .unwrap()
                .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
            let tracking = swaps.tracking.entry(operation).or_default();
            tracking.setup = Some(setup);
            tracking.orderbook = Some(orderbook);
            assert_eq!(
                swaps.stage(swaps.record(operation).unwrap()),
                SwapStage::Approved
            );
        });
        swaps
    })
}

/// A new swap of 1 USDC for USDT to the private balance, planned from a 10 USDC stub note and
/// quoted through the stub orderbook.
fn open_private_form(
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
        });
        swaps.update(cx, |swaps, cx| {
            swaps
                .private_owner()
                .unwrap()
                .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
            swaps.open_form(
                None,
                STUB_USDC,
                Some(STUB_USDT),
                Some(U256::from(1_000_000)),
                None,
                SwapDelivery::Reshield,
                window,
                cx,
            );
            swaps.form.as_mut().unwrap().orderbook = Some(orderbook);
            swaps.schedule_quote(window, cx);
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

/// Emit the gas slider's `Change` to `value`, or its `Release` when `release`, as a drag does.
fn move_gas_slider(
    swaps: &Entity<PrivateSwapsView>,
    value: f32,
    release: bool,
    cx: &mut gpui::VisualTestContext,
) {
    use gpui_component::slider::SliderValue;
    cx.update(|_, cx| {
        let slider = swaps.read(cx).form.as_ref().unwrap().gas_slider.clone();
        slider.update(cx, |_, cx| {
            cx.emit(if release {
                SliderEvent::Release(SliderValue::Single(value))
            } else {
                SliderEvent::Change(SliderValue::Single(value))
            });
        });
    });
    cx.run_until_parked();
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

/// The review's Gas row: the gas the user pays of the gas estimate, in money.
fn gas_row(swaps: &PrivateSwapsView, review: &SwapReview, cx: &App) -> (String, String) {
    let buy = review.plan().buy_token();
    (
        "Gas".to_owned(),
        format!(
            "{} · up to {} of ≈ {}",
            gas_share_name(review.gas_share_bps()),
            swaps.gas_money(buy, review.gas_allowance(), cx),
            swaps.gas_money(buy, review.gas_estimate(), cx)
        ),
    )
}

fn configure_public_review_assets(root: &mut WalletRoot) {
    for account in &mut Arc::make_mut(root.public_balance_snapshot.as_mut().unwrap()).accounts {
        for balance in &mut account.balances {
            let token = match balance.asset.id {
                wallet_ops::PublicAssetId::Erc20(token) if token == Address::repeat_byte(42) => {
                    Some((STUB_USDC, "USDC"))
                }
                wallet_ops::PublicAssetId::Erc20(token) if token == Address::repeat_byte(43) => {
                    Some((STUB_USDT, "USDT"))
                }
                _ => None,
            };
            if let Some((token, symbol)) = token {
                balance.asset.id = wallet_ops::PublicAssetId::Erc20(token);
                balance.asset.symbol = symbol.into();
                balance.asset.decimals = 6;
                balance.amount = wallet_ops::PublicBalanceAmount::Available(U256::from(10_000_000));
            }
        }
    }
}

/// Build the backend's actual deposit or order review; only the network responses are stubbed.
#[allow(clippy::too_many_arguments)]
fn public_review_fixture(
    root: &Entity<WalletRoot>,
    session: &Arc<WalletSession>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    sell: Address,
    sell_amount: U256,
    order: bool,
    cx: &gpui::VisualTestContext,
) -> (
    wallet_ops::PublicSwapReview,
    wallet_ops::bridge::AcrossClient,
    CowOrderbookClient,
    wallet_ops::bridge::PublicBridgeDestination,
) {
    use wallet_ops::bridge::{
        AcrossClient, AcrossRoute, PublicSellAsset, public_across_destination_tokens,
    };
    let orderbook = stub_orderbook(stubs, runtime);
    let across =
        AcrossClient::new(orderbook.http().clone(), stubs.url.join("across").unwrap()).unwrap();
    let (origin, registry, source) = root.read_with(cx, |root, _| {
        (
            root.effective_chain_configs.get(1).unwrap().clone(),
            root.effective_token_registry.clone(),
            root.public_accounts[1].address,
        )
    });
    let route = public_across_destination_tokens(
        &[AcrossRoute {
            origin_token: if order { STUB_USDC } else { sell },
            destination_token: STUB_POLYGON_USDC,
            origin_symbol: "USDC".into(),
            destination_symbol: "USDC".into(),
        }],
        PublicSellAsset::Erc20(sell),
        true,
        &registry,
        137,
    )
    .remove(0);
    let review = runtime
        .block_on(session.executor_owner().unwrap().review_public_swap(
            wallet_ops::PublicSwapReviewRequest {
                origin: &origin,
                source,
                sell: PublicSellAsset::Erc20(sell),
                sell_amount,
                destination: &route,
                slippage_bps: DEFAULT_SLIPPAGE_BPS,
                gas_share_bps: GAS_SHARE_BALANCED_BPS,
                on_shield_failure: BridgeShieldFailure::default(),
                orderbook: Some(&orderbook),
                across: &across,
                anchor_cache: None,
                token_registry: &registry,
                max_fee_per_gas: 1,
                max_priority_fee_per_gas: 1,
            },
        ))
        .unwrap();
    (review, across, orderbook, route)
}

/// Select through the production menu so disabled rows and owner subscriptions are exercised.
fn choose_swap_source(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    if cx.debug_bounds(selector).is_none() {
        let trigger = cx
            .debug_bounds("swap-pay-from")
            .unwrap_or_else(|| panic!("Pay from control before {selector}"));
        cx.simulate_click(trigger.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    let row = cx.debug_bounds(selector).expect("source menu row");
    cx.simulate_click(row.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

/// Click the rendered Pay from control and report whether its menu opened.
fn pay_from_menu_opens(cx: &mut gpui::VisualTestContext) -> bool {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let trigger = cx.debug_bounds("swap-pay-from").expect("Pay from control");
    cx.simulate_click(trigger.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.debug_bounds("swap-source-private").is_some()
}

/// A chosen stealth account spends the private balance, so Pay from is off until the form is
/// back on a new account, and a choice the control committed anyway is put back.
#[gpui::test]
fn pay_from_is_fixed_while_an_existing_stealth_account_is_chosen(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, _, _, cx| {
            reusable_account(executors, operation);
            cx.update(|window, cx| {
                root.update(cx, |root, _| {
                    enable_stub_chain(root, &stubs, 1);
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    configure_public_review_assets(root);
                });
                swaps.update(cx, |swaps, cx| {
                    swaps.open_new_form(STUB_USDC, window, cx);
                    swaps.select_form_account(Some(operation), window, cx);
                });
            });
            let swap_use = swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.operation, Some(operation));
                assert!(form.reuse_use.is_some());
                form.reuse_use
            });

            assert!(
                !pay_from_menu_opens(cx),
                "the payer can't change while an existing account is chosen"
            );
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert!(form.public.is_none());
                assert_eq!(form.pay_from_select.read(cx).selected_value(), Some(&None));
            });

            // The control commits its choice before it reports it to the form.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let select = swaps.form.as_ref().unwrap().pay_from_select.clone();
                    let public = Some("account-1".to_owned());
                    select.update(cx, |select, cx| {
                        select.set_selected_value(&public, window, cx);
                    });
                    assert_eq!(select.read(cx).selected_value(), Some(&public));
                    swaps.select_form_source(public.as_deref(), window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert!(form.public.is_none());
                    assert_eq!(
                        form.pay_from_select.read(cx).selected_value(),
                        Some(&None),
                        "a rejected payer doesn't stay shown"
                    );
                    assert_eq!(
                        (form.operation, form.reuse_use),
                        (Some(operation), swap_use)
                    );

                    swaps.select_form_account(None, window, cx);
                });
            });
            choose_swap_source(cx, "swap-source-public-account-1");
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    form.public
                        .as_ref()
                        .map(|public| public.account.public_account_uuid.as_str()),
                    Some("account-1"),
                    "a new account lets a Public account pay"
                );
            });
        },
    );
}

#[gpui::test]
fn pay_from_changes_clear_acknowledgements_and_global_imports_cannot_be_chosen(
    cx: &mut TestAppContext,
) {
    let stubs = SwapStubs::start();
    stubs.enable_public_reviews();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            let sell = STUB_USDC;
            cx.update(|window, cx| {
                root.update(cx, |root, _| {
                    root.public_accounts.truncate(4);
                    root.public_accounts[3].source =
                        wallet_ops::vault::PublicAccountSource::Imported;
                    root.public_accounts[3].scope = wallet_ops::vault::PublicAccountScope::Global;
                    enable_stub_chain(root, &stubs, 1);
                    enable_stub_chain(root, &stubs, 137);
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    configure_public_review_assets(root);
                });
                swaps.update(cx, |swaps, cx| {
                    swaps.open_new_form(sell, window, cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.amount_input
                        .update(cx, |input, cx| input.set_value("7", window, cx));
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.destination_route.fee_token = Some(sell);
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
            let (review, across, orderbook, route) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                sell,
                U256::from(7_000_000),
                false,
                cx,
            );
            choose_swap_source(cx, "swap-source-public-account-1");
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    form.public.as_ref().unwrap().account.public_account_uuid,
                    "account-1"
                );
                assert_eq!(
                    form.sell, sell,
                    "the account holds the previously selected Sell token"
                );
                assert_eq!(form.amount_input.read(cx).value().as_ref(), "7");
                assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                assert!(
                    form.account_select.is_none(),
                    "a Public source creates no source stealth account"
                );
                assert!(form.destination_select.is_some());
                assert!(
                    !form.destination_route.is_used(),
                    "another payer must review setup terms again"
                );
                assert_eq!(form.assets.locked, U256::ZERO);
                assert!(
                    form.assets
                        .sell_assets
                        .iter()
                        .any(|asset| asset.token == Address::ZERO)
                );
                assert!(swaps.receive_to_locked(form));
            });
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.network = Some(137);
                    form.buy = Some(STUB_POLYGON_USDC);
                    form.destination_account = Some(DestinationAccount {
                        chain_id: 137,
                        operation: destination_operation,
                        index: destination_record.index(),
                        address: destination_record.address().unwrap(),
                    });
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.public
                        .as_mut()
                        .unwrap()
                        .routes
                        .insert((sell, 137), vec![route]);
                    swaps.install_public_review_for_tests(
                        review.clone(),
                        across.clone(),
                        Some(orderbook.clone()),
                        cx,
                    );
                    assert!(
                        swaps
                            .form
                            .as_ref()
                            .unwrap()
                            .public
                            .as_ref()
                            .unwrap()
                            .review
                            .is_some()
                    );
                });
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.set_form_sell(STUB_USDT, window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(
                        (form.network, form.buy),
                        (Some(137), Some(STUB_POLYGON_USDC))
                    );
                    assert!(
                        form.public.as_ref().unwrap().review.is_none(),
                        "an unserved Sell pair cannot retain the previous token's received minimum"
                    );
                    assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                    swaps.request_public_review(window, cx);
                    assert!(
                        swaps.public_authorization.is_none(),
                        "Review waits for the new pair's route"
                    );
                    swaps.set_form_sell(sell, window, cx);
                    swaps.install_public_review_for_tests(review, across, Some(orderbook), cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                });
            });
            choose_swap_source(cx, "swap-source-public-account-2");
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                let public = form.public.as_ref().unwrap();
                assert_eq!(public.account.public_account_uuid, "account-2");
                assert!(public.review.is_none() && public.routes.is_empty());
                assert_eq!(
                    (form.network, form.buy),
                    (Some(137), Some(STUB_POLYGON_USDC)),
                    "a payer change preserves the chosen destination without its old quote"
                );
                assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| swaps.request_public_review(window, cx));
            });
            swaps.read_with(cx, |swaps, _| {
                assert!(
                    swaps.public_authorization.is_none(),
                    "the new payer has no route and cannot approve the previous payer's minimum"
                );
            });
            choose_swap_source(cx, "swap-source-public-account-3");
            swaps.read_with(cx, |swaps, _| {
                assert_eq!(
                    swaps
                        .form
                        .as_ref()
                        .unwrap()
                        .public
                        .as_ref()
                        .unwrap()
                        .account
                        .public_account_uuid,
                    "account-2",
                    "a globally visible import cannot admit a Public swap"
                );
            });
            swaps.read_with(cx, |swaps, cx| {
                let groups = swaps.swap_source_items(sell, cx);
                // Private balance, the heading of the Public accounts, then the accounts.
                assert_eq!(groups.len(), 1);
                assert!(groups[0].items[0].account.is_none());
                assert!(groups[0].items[1].heading && groups[0].items[1].unavailable);
                let (shared, scoped): (Vec<_>, Vec<_>) = groups[0].items[2..]
                    .iter()
                    .partition(|item| item.account.as_deref() == Some("account-3"));
                assert!(shared[0].unavailable);
                assert_eq!(
                    shared[0].reason.as_deref(),
                    Some(SHARED_ACCOUNT_REASON),
                    "Pay from says why an account shared between Private wallets can't pay"
                );
                assert!(
                    scoped
                        .iter()
                        .all(|item| !item.unavailable && item.reason.is_none())
                );
                // An account whose balances are read holds none of a token they don't list.
                let unheld = Address::repeat_byte(0x77);
                let zero = format!("0 {}", swaps.token_symbol(unheld, cx));
                let read = root
                    .read(cx)
                    .public_balance_snapshot
                    .as_ref()
                    .unwrap()
                    .accounts
                    .iter()
                    .map(|account| account.account.public_account_uuid.clone())
                    .collect::<Vec<_>>();
                let groups = swaps.swap_source_items(unheld, cx);
                let rows = groups[0]
                    .items
                    .iter()
                    .filter(|item| {
                        item.account
                            .as_ref()
                            .is_some_and(|uuid| read.contains(uuid))
                    })
                    .collect::<Vec<_>>();
                assert!(
                    !rows.is_empty() && rows.iter().all(|item| item.balance == zero),
                    "a read account without the Sell token shows a zero balance"
                );
            });
            choose_swap_source(cx, "swap-source-private");
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert!(form.public.is_none());
                assert!(
                    form.account_select.is_some(),
                    "Private balance retains the existing account flow"
                );
                assert_eq!(form.receive_to, ReceiveTo::PrivateBalance);
                assert!(!swaps.receive_to_locked(form));
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

#[gpui::test]
fn public_bridge_opens_before_a_destination_or_origin_private_session_exists(
    cx: &mut TestAppContext,
) {
    with_swap_view(cx, |root, _, _, _, _, cx| {
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                root.clear_private_swaps(cx);
                root.chain_states.remove(&1);
                root.open_public_swap_form("account-1", Some(Address::repeat_byte(42)), window, cx);
            });
        });
        cx.run_until_parked();
        let swaps = root.read_with(cx, |root, _| root.private_swaps_view().unwrap());
        swaps.read_with(cx, |swaps, _| {
            assert!(swaps.session.is_none() && swaps.owner.is_none());
            assert_eq!(swaps.origin_chain_id, 1);
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(
                form.public.as_ref().unwrap().account.public_account_uuid,
                "account-1"
            );
            assert!(
                form.network.is_none(),
                "opening does not bind an arbitrary destination owner"
            );
            assert!(form.account_select.is_none());
            assert!(form.destination_select.is_some());
        });
        choose_swap_source(cx, "swap-source-private");
        swaps.read_with(cx, |swaps, _| {
            assert!(
                swaps.form.as_ref().unwrap().public.is_some(),
                "Private balance cannot be selected without a source private session"
            );
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
    });
}

#[gpui::test]
fn public_route_error_retry_discards_the_client_and_preserves_the_setup(cx: &mut TestAppContext) {
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
            let (destination_store, operation) = reusable_polygon_account(root, cx);
            let destination = destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap();
            let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let (review, across, orderbook, _) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                STUB_USDT,
                U256::from(7_000_000),
                true,
                cx,
            );
            let use_id = SwapUseId::random().unwrap();
            let revision = cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(source, STUB_USDT, window, cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.network = Some(137);
                    form.buy = Some(STUB_POLYGON_USDC);
                    form.amount_input
                        .update(cx, |input, cx| input.set_value("7", window, cx));
                    swaps.install_public_review_for_tests(review, across, Some(orderbook), cx);
                    let form = swaps.form.as_mut().unwrap();
                    let public = form.public.as_mut().unwrap();
                    public.operation = Some(operation);
                    public.swap_use = Some(use_id);
                    public.route_tasks.clear();
                    public.routes.clear();
                    public.route_errors.insert(
                        (STUB_USDT, 137),
                        "Across rejected the routes request (HTTP 403).".into(),
                    );
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.quote_task =
                        Some(cx.spawn(async move |_, _| std::future::pending::<()>().await));
                    cx.notify();
                    form.quote_revision
                })
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let retry = cx
                .debug_bounds("swap-bridge-routes-retry")
                .expect("the route error offers Retry");
            cx.simulate_click(retry.center(), gpui::Modifiers::none());
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                let public = form.public.as_ref().unwrap();
                assert!(
                    public.quote_route_is_invalidated_for_tests(),
                    "Retry discards the isolated operation client and old review"
                );
                assert!(
                    form.quote_task.is_none() && form.quote_revision != revision,
                    "an old preview cannot restore the discarded client"
                );
                assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                assert_eq!(
                    (public.operation, public.swap_use),
                    (Some(operation), Some(use_id))
                );
                assert_eq!(
                    (form.sell, form.network, form.buy),
                    (STUB_USDT, Some(137), Some(STUB_POLYGON_USDC))
                );
                assert_eq!(form.amount_input.read(cx).value().as_ref(), "7");
                assert!(public.route_errors.is_empty());
                assert!(
                    public.route_tasks.contains_key(&(STUB_USDT, 137)),
                    "Retry restarts route discovery"
                );
            });
            assert_eq!(
                destination_store
                    .records()
                    .unwrap()
                    .into_iter()
                    .find(|record| record.operation() == operation)
                    .unwrap(),
                destination
            );
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, _| {
                    swaps
                        .form
                        .as_mut()
                        .unwrap()
                        .public
                        .as_mut()
                        .unwrap()
                        .route_tasks
                        .clear();
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

#[gpui::test]
fn changed_public_delivery_keeps_the_corrected_review_until_the_draft_changes(
    cx: &mut TestAppContext,
) {
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
            let (destination_store, operation) = reusable_polygon_account(root, cx);
            let destination = destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap();
            let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let (review, across, orderbook, route) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                STUB_USDT,
                U256::from(7_000_000),
                true,
                cx,
            );
            let original_gas_maximum = review.gas_plan().max_gas_cost;
            assert!(!original_gas_maximum.is_zero());
            let original_minimum = review.bridge().received_minimum();
            let original_destination_minimum = review.bridge().destination_minimum;
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(source, STUB_USDT, window, cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.network = Some(137);
                    form.buy = Some(STUB_POLYGON_USDC);
                    form.amount_input
                        .update(cx, |input, cx| input.set_value("7", window, cx));
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
                        .insert((STUB_USDT, 137), vec![route.clone()]);
                    swaps.install_public_review_for_tests(review, across, Some(orderbook), cx);
                    swaps.form.as_mut().unwrap().price_acknowledged = true;
                    swaps.request_public_review(window, cx);
                });
            });
            let command =
                swaps.read_with(cx, |swaps, _| swaps.public_authorization.clone().unwrap());
            cx.update(WindowExt::close_dialog);
            // The backend regression owns actual-fee derivation and signing. This real review
            // with changed fees exercises the UI's production Changed restoration path.
            stubs.set_across_fee_bps(50);
            // Approval completed before signing found the lower bridge minimum. The next
            // review must charge only the remaining work, so this balance can continue.
            stubs
                .public_allowance_complete
                .store(true, std::sync::atomic::Ordering::Relaxed);
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    let snapshot = Arc::make_mut(root.public_balance_snapshot.as_mut().unwrap());
                    for balance in snapshot
                        .accounts
                        .iter_mut()
                        .flat_map(|account| &mut account.balances)
                    {
                        if balance.asset.id == wallet_ops::PublicAssetId::Native {
                            balance.amount = wallet_ops::PublicBalanceAmount::Available(
                                original_gas_maximum - U256::ONE,
                            );
                        }
                    }
                });
            });
            let (corrected, _, _, _) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                STUB_USDT,
                U256::from(7_000_000),
                true,
                cx,
            );
            assert!(corrected.gas_plan().approval_gas_limits.is_empty());
            assert!(corrected.gas_plan().max_gas_cost < original_gas_maximum);
            let corrected_minimum = corrected.bridge().received_minimum();
            assert!(corrected_minimum < original_minimum);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps
                        .return_public_review_change_for_tests(&command, corrected, window, cx)
                        .unwrap();
                    // A late route-list refresh must preserve the signing-time review.
                    swaps
                        .form
                        .as_mut()
                        .unwrap()
                        .public
                        .as_mut()
                        .unwrap()
                        .routes
                        .insert((STUB_USDT, 137), vec![route]);
                    swaps.refresh_form_delivery(cx);
                    swaps.schedule_public_quote(window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(
                        form.public
                            .as_ref()
                            .unwrap()
                            .review
                            .as_ref()
                            .unwrap()
                            .bridge()
                            .received_minimum(),
                        corrected_minimum
                    );
                    assert!(form.quote_task.is_none());
                    assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                    assert!(swaps.public_authorization.is_none());
                });
            });
            let held = destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap();
            assert_eq!(
                held.issued(),
                destination.issued(),
                "reapproval must reuse the completed setup"
            );
            assert_eq!(
                held.swap_uses()[0]
                    .public_swap()
                    .unwrap()
                    .approval()
                    .bounds
                    .destination_minimum,
                Some(original_destination_minimum),
                "the corrected terms need explicit consent before persistence"
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.form.as_mut().unwrap().price_acknowledged = true;
                    swaps.request_public_review(window, cx);
                    assert!(swaps.public_authorization.is_some(), "completed approvals must not block the corrected review with an obsolete gas budget");
                });
            });
            cx.update(|window, cx| {
                use gpui_kit::test::TestWindowExt;
                window.click("wallet-spend-auth-cancel", cx);
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.set_form_sell(STUB_USDC, window, cx);
                    assert!(
                        swaps
                            .form
                            .as_ref()
                            .unwrap()
                            .public
                            .as_ref()
                            .unwrap()
                            .review
                            .is_none(),
                        "editing the pair discards the corrected review"
                    );
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// An order whose approval is a signed permit asks the Public account for no gas: with no
/// native balance it reaches its review, which says the approval is signed, and it stays
/// approvable when signing returns it with a corrected delivery.
#[gpui::test]
fn permit_public_order_needs_no_native_balance_through_a_corrected_delivery(
    cx: &mut TestAppContext,
) {
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
                    let snapshot = Arc::make_mut(root.public_balance_snapshot.as_mut().unwrap());
                    for balance in snapshot
                        .accounts
                        .iter_mut()
                        .flat_map(|account| &mut account.balances)
                    {
                        if balance.asset.id == wallet_ops::PublicAssetId::Native {
                            balance.amount = wallet_ops::PublicBalanceAmount::Available(U256::ZERO);
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
            let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let label = public_source_label(&source);
            let (review, across, orderbook, route) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                STUB_USDT,
                U256::from(7_000_000),
                true,
                cx,
            );
            // The stub token has no permit, so the review is turned into the one a token with
            // a permit gets.
            let review = review.signing_permit_for_tests();
            let original_minimum = review.bridge().received_minimum();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(source, STUB_USDT, window, cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.network = Some(137);
                    form.buy = Some(STUB_POLYGON_USDC);
                    form.amount_input
                        .update(cx, |input, cx| input.set_value("7", window, cx));
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
                        .insert((STUB_USDT, 137), vec![route.clone()]);
                    swaps.install_public_review_for_tests(
                        review.clone(),
                        across,
                        Some(orderbook),
                        cx,
                    );
                    let form = swaps.form.as_ref().unwrap();
                    assert!(
                        swaps
                            .public_form_reason(form, cx)
                            .is_none_or(|reason| !reason.blocks_review),
                        "a signed approval needs no gas from the account"
                    );
                    let details = swaps.public_details(form, &review, cx);
                    let approval = details
                        .iter()
                        .find(|row| row.label == format!("Approval from {label}"))
                        .expect("the Costs details name the approval");
                    assert_eq!(
                        approval.text_for_test(),
                        Some(("signed, not sent", Some("no gas")))
                    );
                    swaps.form.as_mut().unwrap().price_acknowledged = true;
                    swaps.request_public_review(window, cx);
                    assert!(
                        swaps.public_authorization.is_some(),
                        "{:?}",
                        swaps.form.as_ref().unwrap().error
                    );
                });
            });
            let command =
                swaps.read_with(cx, |swaps, _| swaps.public_authorization.clone().unwrap());
            let summary = command.public_authorization_summary();
            let rows = summary.rows_for_test();
            assert!(
                !rows.iter().any(|(row, _)| row == "Pay now"),
                "nothing is paid now by the account: {rows:?}"
            );
            assert!(
                rows.contains(&(
                    "Approval".to_owned(),
                    format!("signed by {label}, not sent · no gas")
                )),
                "{rows:?}"
            );
            let (_, collapsed, costs) = summary.row_group_for_test().unwrap();
            assert!(costs.contains(&"Approval".to_owned()), "{costs:?}");
            assert!(!collapsed.contains("now, not refunded"), "{collapsed}");
            let signed = summary.row_hint_for_test("Approval").unwrap();
            assert!(
                signed.contains(&format!(
                    "{label} signs an approval that lets CoW take exactly"
                )) && signed.contains("It sends no transaction for it and pays no gas."),
                "{signed}"
            );
            cx.update(WindowExt::close_dialog);
            // Signing found a lower bridge minimum. The corrected review still signs the permit.
            stubs.set_across_fee_bps(50);
            let (corrected, _, _, _) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                STUB_USDT,
                U256::from(7_000_000),
                true,
                cx,
            );
            let corrected = corrected.signing_permit_for_tests();
            let corrected_minimum = corrected.bridge().received_minimum();
            assert!(corrected_minimum < original_minimum);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps
                        .return_public_review_change_for_tests(&command, corrected, window, cx)
                        .unwrap();
                    // A late route-list refresh must preserve the signing-time review.
                    swaps
                        .form
                        .as_mut()
                        .unwrap()
                        .public
                        .as_mut()
                        .unwrap()
                        .routes
                        .insert((STUB_USDT, 137), vec![route]);
                    swaps.refresh_form_delivery(cx);
                    swaps.schedule_public_quote(window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    let shown = form.public.as_ref().unwrap().review.as_ref().unwrap();
                    assert_eq!(shown.bridge().received_minimum(), corrected_minimum);
                    assert!(shown.signs_permit());
                    assert!(form.quote_task.is_none());
                    swaps.form.as_mut().unwrap().price_acknowledged = true;
                    swaps.request_public_review(window, cx);
                    assert!(
                        swaps.public_authorization.is_some(),
                        "the corrected permit review needs no native balance: {:?}",
                        swaps.form.as_ref().unwrap().error
                    );
                });
            });
            cx.update(|window, cx| {
                use gpui_kit::test::TestWindowExt;
                window.click("wallet-spend-auth-cancel", cx);
            });
            cx.run_until_parked();
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

#[gpui::test]
fn closing_public_preparation_aborts_continuation_and_continue_keeps_the_durable_use(
    cx: &mut TestAppContext,
) {
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
            let issued_setup = destination_record.issued()[0].hash();
            let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let sell = STUB_USDC;
            let (review, across, orderbook, route) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                sell,
                U256::from(7_000_000),
                false,
                cx,
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(source, sell, window, cx);
                    let form = swaps.form.as_mut().unwrap();
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
                    swaps.install_public_review_for_tests(review, across, Some(orderbook), cx);
                    swaps.form.as_mut().unwrap().price_acknowledged = true;
                    swaps.request_public_review(window, cx);
                });
            });
            let command =
                swaps.read_with(cx, |swaps, _| swaps.public_authorization.clone().unwrap());
            cx.update(WindowExt::close_dialog);
            let (release, gate) = tokio::sync::oneshot::channel();
            let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let identity = cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps
                        .hold_public_preparation_for_tests(
                            command,
                            gate,
                            completed.clone(),
                            window,
                            cx,
                        )
                        .unwrap()
                })
            });
            cx.run_until_parked();
            runtime.block_on(tokio::task::yield_now());
            assert!(
                !release.is_closed(),
                "the preparation is suspended before its continuation"
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.navigate(SwapDialogView::Orders, window, cx);
                });
            });
            cx.run_until_parked();
            assert_eq!(
                completed.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "a dismissed preparation must not reach its signing/submission continuation"
            );
            swaps.read_with(cx, |swaps, _| {
                assert!(
                    swaps.public_job.is_none()
                        && swaps.public_authorization.is_none()
                        && swaps.public_execution.is_none()
                );
                assert!(
                    swaps
                        .dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.view == SwapDialogView::Orders),
                    "My orders dismisses the form's preparation without changing its claim"
                );
            });
            let claimed = polygon
                .executor_owner()
                .unwrap()
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == identity.operation)
                .unwrap();
            assert!(
                claimed.public_swap_use(identity.swap_use).is_some(),
                "closing keeps the encrypted claim for recovery"
            );
            assert_eq!(
                (claimed.issued().len(), claimed.issued()[0].hash()),
                (1, issued_setup)
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.refresh_public_swap_records(cx);
                    swaps.perform_public_swap_action(
                        identity,
                        super::super::public_progress::PublicSwapAction::Continue,
                        window,
                        cx,
                    );
                    let public = swaps.form.as_ref().unwrap().public.as_ref().unwrap();
                    assert_eq!(
                        (public.operation, public.swap_use),
                        (Some(identity.operation), Some(identity.swap_use))
                    );
                    assert!(
                        public.review.is_none() && swaps.public_authorization.is_none(),
                        "Continue requires a fresh review"
                    );
                });
            });
            let (review, across, orderbook, _) = public_review_fixture(
                root,
                &polygon,
                &stubs,
                runtime,
                sell,
                U256::from(7_000_000),
                false,
                cx,
            );
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.install_public_review_for_tests(review, across, Some(orderbook), cx);
                    swaps.form.as_mut().unwrap().price_acknowledged = true;
                    swaps.request_public_review(window, cx);
                });
            });
            let resumed_command =
                swaps.read_with(cx, |swaps, _| swaps.public_authorization.clone().unwrap());
            cx.update(WindowExt::close_dialog);
            let (next_release, next_gate) = tokio::sync::oneshot::channel();
            let next_completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let resumed_identity = swaps
                        .hold_public_preparation_for_tests(
                            resumed_command,
                            next_gate,
                            next_completed.clone(),
                            window,
                            cx,
                        )
                        .unwrap();
                    assert_eq!(
                        resumed_identity, identity,
                        "a new review continues the original use"
                    );
                });
            });
            cx.run_until_parked();
            runtime.block_on(tokio::task::yield_now());
            let _ = release.send(());
            runtime.block_on(tokio::task::yield_now());
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, _| {
                assert!(
                    swaps.public_job.is_some(),
                    "an abandoned task cannot clear a newer preparation"
                );
                assert!(
                    swaps.public_authorization.is_none() && swaps.public_execution.is_none(),
                    "an abandoned result cannot restore authority into the continued form"
                );
            });
            assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(next_completed.load(std::sync::atomic::Ordering::SeqCst), 0);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.refresh_public_swap_records(cx);
                    swaps.navigate(SwapDialogView::PublicDetail(identity), window, cx);
                });
            });
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, _| {
                assert!(
                    swaps.public_job.is_some() && swaps.form.is_none(),
                    "the claimed swap's detail takes its preparation over from the form"
                );
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| swaps.close_swap_dialog(window, cx));
            });
            cx.run_until_parked();
            let _ = next_release.send(());
            runtime.block_on(tokio::task::yield_now());
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, _| {
                assert!(
                    swaps.public_job.is_none()
                        && swaps.public_authorization.is_none()
                        && swaps.public_execution.is_none()
                );
                assert!(
                    swaps.form.is_none() && swaps.dialog.is_none(),
                    "closing cannot repopulate a preparation"
                );
            });
            assert_eq!(
                next_completed.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "closing the detail at work stops its signing/submission continuation"
            );
            let resumed = polygon
                .executor_owner()
                .unwrap()
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == identity.operation)
                .unwrap();
            assert_eq!(
                resumed.swap_uses().len(),
                claimed.swap_uses().len(),
                "Continue does not create another use"
            );
            assert_eq!(
                (resumed.issued().len(), resumed.issued()[0].hash()),
                (1, issued_setup),
                "Continue retains the confirmed setup without issuing another payload"
            );
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

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
                        super::super::public_progress::PublicSwapAction::Continue,
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
                            receive_to_problem(fresh, cx).is_none()
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
                        swaps.form.as_mut().unwrap().price_acknowledged = true;
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
                                    Some(U256::from(50_000)),
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
                Some(U256::from(50_000)),
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

#[gpui::test]
fn public_buy_picker_keeps_same_asset_routes_and_refuses_own_chain_and_unfunded_destination(
    cx: &mut TestAppContext,
) {
    use alloy::primitives::address;
    use wallet_ops::bridge::{
        AcrossRoute, PublicBridgePath, PublicSellAsset, public_across_destination_tokens,
    };
    let sell = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let received = address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359");
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            let account = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    enable_stub_chain(root, &stubs, 137);
                });
            });
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(account, sell, window, cx);
                    let root = swaps.root.upgrade().unwrap();
                    let routes = public_across_destination_tokens(
                        &[AcrossRoute {
                            origin_token: sell,
                            destination_token: received,
                            origin_symbol: "USDC".into(),
                            destination_symbol: "USDC".into(),
                        }],
                        PublicSellAsset::Erc20(sell),
                        false,
                        &root.read(cx).effective_token_registry,
                        137,
                    );
                    assert_eq!(routes[0].path, PublicBridgePath::Deposit);
                    let form = swaps.form.as_mut().unwrap();
                    form.public
                        .as_mut()
                        .unwrap()
                        .routes
                        .insert((sell, 137), routes);
                    form.picker.network = 137;
                    form.picker.open = true;
                    let form = swaps.form.as_ref().unwrap();
                    let items = swaps.buy_picker_items(form, cx);
                    assert!(
                        items.iter().any(|item| item.asset.token == received),
                        "a direct route remains selectable when there is no CoW route"
                    );
                    assert_eq!(
                        swaps.network_availability(ReceiveTo::PrivateBalance, 1, cx),
                        Some(NetworkAvailability::Unavailable(NetworkUnavailable::Shield))
                    );
                    assert_eq!(
                        swaps.network_availability(ReceiveTo::PrivateBalance, 137, cx),
                        Some(NetworkAvailability::Unavailable(
                            NetworkUnavailable::Unfunded
                        ))
                    );
                    swaps.show_buy_picker_network(1, window, cx);
                    assert_eq!(
                        swaps.form.as_ref().unwrap().picker.network,
                        137,
                        "the own-chain Shield pointer cannot become a bridge destination"
                    );
                    let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
                    assert!(
                        content.receive_to == ReceiveTo::PrivateBalance
                            && content.receive_to_locked,
                        "the switch shows Private selected and can't change"
                    );
                    swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                    assert_eq!(
                        swaps.form.as_ref().unwrap().receive_to,
                        ReceiveTo::PrivateBalance
                    );
                    swaps.form.as_mut().unwrap().picker.open = false;
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

// A chain with a Public swap profile takes no orders until its math contract is deployed. Its
// form then lists what a deposit serves and says so without blocking Review, as on a chain
// that has no profile. A form opened once the contract is there lists the other tokens too.
#[gpui::test]
fn public_form_lists_only_direct_routes_until_its_chain_takes_orders(cx: &mut TestAppContext) {
    use wallet_ops::bridge::PublicBridgePath;
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
            let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let key = (STUB_USDT, 137);
            for available in [false, true] {
                stubs.set_math_deployed(available);
                // Each form reads its chain again.
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.open_public_form(source.clone(), STUB_USDT, window, cx);
                        swaps.form.as_mut().unwrap().network = Some(137);
                        swaps.load_bridge_routes(window, cx);
                    });
                });
                drive_until(cx, runtime, |cx| {
                    swaps.read_with(cx, |swaps, _| {
                        let public = swaps.form.as_ref().unwrap().public.as_ref().unwrap();
                        public.orders_available.is_some() && public.routes.contains_key(&key)
                    })
                });
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        let form = swaps.form.as_mut().unwrap();
                        let public = form.public.as_ref().unwrap();
                        assert_eq!(public.orders_available, Some(available));
                        let listed = public.routes[&key]
                            .iter()
                            .map(|route| (route.destination.destination_token, route.path))
                            .collect::<Vec<_>>();
                        let direct = (STUB_POLYGON_USDT, PublicBridgePath::Deposit);
                        if available {
                            // Every other token Across delivers there is listed as an order.
                            assert!(listed.contains(&direct));
                            assert!(listed.contains(&(STUB_POLYGON_USDC, PublicBridgePath::Order)));
                            assert!(listed.iter().all(|route| *route == direct
                                || route.1 == PublicBridgePath::Order));
                        } else {
                            assert_eq!(listed, [direct]);
                        }
                        // The token that bridges directly can be picked either way.
                        form.buy = Some(STUB_POLYGON_USDT);
                        let reason = swaps.public_form_reason(swaps.form.as_ref().unwrap(), cx);
                        if available {
                            assert!(reason.is_none());
                        } else {
                            let reason = reason.expect("a chain without orders says so");
                            assert!(!reason.blocks_review);
                            assert_eq!(
                                reason.text,
                                format!(
                                    "From {}, a token can only be bridged as itself, such as USDC to USDC. Swapping to a different token isn't supported on this network.",
                                    network_name(1)
                                )
                            );
                        }
                        swaps.close_swap_dialog(window, cx);
                    });
                });
                cx.run_until_parked();
            }
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

#[gpui::test]
fn held_public_destination_shows_the_held_amount_and_the_recovery_action(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::Bytes;
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::{
        Call, CommitmentPreimage, RelayAdapt7702, ShieldCiphertext, ShieldRequest, TokenData,
        shieldCall,
    };
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorNonceWatermark, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload, PublicSwapObservations, SwapBridgeHandoff,
        SwapBridgeOutcome, SwapObservation,
    };
    with_swap_view(cx, |_, swaps, _, _, _, cx| {
        let observed = SwapObservation {
            block: BlockNumHash::new(20, B256::ZERO),
            transaction_hash: Some(B256::ZERO),
        };
        let held = PublicSwapObservations {
            bridge_handoff: Some(SwapBridgeHandoff {
                observation: observed,
                deposit_id: Some(U256::ONE),
            }),
            bridge_outcome: Some(SwapBridgeOutcome::HeldOnDestination {
                block: observed.block,
                transaction_hash: B256::ZERO,
                amount: U256::from(100),
            }),
            ..Default::default()
        };
        let record = model::tests::public_presentation_record(true, &held);
        let identity = model::SwapIdentity {
            operation: record.operation(),
            swap_use: record.swap_uses()[0].id(),
        };
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.public_records = vec![(1, record.clone())];
                swaps.show_view(SwapDialogView::PublicDetail(identity), window, cx);
            });
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            let (record, claimed, swap) = swaps.public_swap_record(identity).unwrap();
            let stage = model::public_swap_stage(
                record,
                claimed,
                None,
                now_unix(),
                swaps.attribution(record),
                swaps.public_destination_balance(record, claimed),
            )
            .unwrap();
            assert_eq!(stage, model::PublicSwapStage::HeldOnDestination);
            // The wallet has no metadata for the held token: the detail keeps the held
            // amount as a private swap words it, and the card's title names only the token.
            let held = swaps.public_labels(record, claimed, cx).unwrap().held;
            assert!(held.as_deref().is_some_and(|held| held.starts_with("100 ")));
            let title = swaps.public_card_line(cx).unwrap().title;
            let held_on = format!(" held on {}", network_name(1));
            assert!(
                !title.starts_with("100") && title.ends_with(&held_on),
                "{title}"
            );
            assert!(
                swaps
                    .shown_public_swaps()
                    .any(|(shown, _)| shown == identity)
            );
            assert!(
                public_progress::public_swap_actions(swap, claimed, stage, now_unix())
                    .contains(&public_progress::PublicSwapAction::RecoverDestination)
            );
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, _| {
            use gpui_kit::test::TestWindowExt;
            assert!(
                window
                    .find("public-swap-action-RecoverDestination")
                    .visible()
            );
        });

        // A completed recovery returns only its requested amount. A different asset's
        // recovery says nothing about these proceeds, even if an explicit check reads zero.
        let recovered = |token: Address| {
            let request = ShieldRequest {
                preimage: CommitmentPreimage {
                    npk: B256::repeat_byte(0x71),
                    token: TokenData::erc20(token),
                    value: alloy::primitives::Uint::from(10_u64),
                },
                ciphertext: ShieldCiphertext {
                    encryptedBundle: [B256::ZERO; 3],
                    shieldKey: B256::ZERO,
                },
            };
            let before =
                ExecutorNonceObservation::new(BlockNumHash::new(29, B256::ZERO), U256::ZERO);
            let calldata = RelayAdapt7702::multicallCall {
                _requireSuccess: true,
                _calls: vec![Call {
                    to: record.address().unwrap(),
                    data: shieldCall {
                        _shieldRequests: vec![request.clone()],
                    }
                    .abi_encode()
                    .into(),
                    value: U256::ZERO,
                }],
                _nonce: U256::ZERO,
                _signature: Bytes::new(),
            }
            .abi_encode();
            let payload = IssuedExecutorPayload::new(
                U256::ZERO,
                record.delegate(),
                B256::repeat_byte(0x71),
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(calldata.into(), before, Vec::new()),
            );
            let mut saved = serde_json::to_value(&record).unwrap();
            saved["issued"] = serde_json::json!([payload]);
            saved["nonce_watermark"] =
                serde_json::to_value(ExecutorNonceWatermark::new(U256::ONE, 31)).unwrap();
            (
                serde_json::from_value::<ExecutorRecord>(saved).unwrap(),
                wallet_ops::ExecutorAttribution::with_shields_for_tests(
                    Address::ZERO,
                    &[(request, 30)],
                ),
            )
        };
        let (partial, relevant) = recovered(Address::repeat_byte(7));
        let (unrelated, other_asset) = recovered(Address::repeat_byte(8));
        let at = |amount, number| Some((U256::from(amount), BlockNumHash::new(number, B256::ZERO)));
        let cases = [
            // No explicit read, including after a failed refresh, keeps recovery available.
            (partial.clone(), relevant.clone(), None, 100, false),
            (partial.clone(), relevant.clone(), at(90_u64, 31), 90, false),
            // A read before the fill isn't a current held amount or completion evidence.
            (partial.clone(), relevant.clone(), at(0_u64, 19), 100, false),
            // Zero before the recovery's shield can't establish that recovery emptied it.
            (partial.clone(), relevant.clone(), at(0_u64, 29), 0, false),
            (unrelated, other_asset, at(0_u64, 31), 0, false),
            (partial, relevant, at(0_u64, 31), 0, true),
        ];
        for (record, attribution, balance, amount, complete) in cases {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.public_records = vec![(1, record)];
                    swaps.attributions.insert(identity.operation, attribution);
                    swaps.public_destination_balances.clear();
                    if let Some(balance) = balance {
                        swaps
                            .public_destination_balances
                            .insert((1, identity.operation, identity.swap_use), balance);
                    }
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            swaps.read_with(cx, |swaps, cx| {
                let (record, claimed, swap) = swaps.public_swap_record(identity).unwrap();
                let stage = model::public_swap_stage(
                    record,
                    claimed,
                    None,
                    now_unix(),
                    swaps.attribution(record),
                    swaps.public_destination_balance(record, claimed),
                )
                .unwrap();
                assert_eq!(
                    stage,
                    if complete {
                        model::PublicSwapStage::Recovered
                    } else {
                        model::PublicSwapStage::HeldOnDestination
                    },
                );
                let label = swaps
                    .public_labels(record, claimed, cx)
                    .unwrap()
                    .held
                    .unwrap();
                assert!(label.starts_with(&format!("{amount} ")), "{label}");
                let actions =
                    public_progress::public_swap_actions(swap, claimed, stage, now_unix());
                assert_eq!(
                    actions.contains(&public_progress::PublicSwapAction::RecoverDestination),
                    !complete,
                );
                assert_eq!(
                    actions.contains(&public_progress::PublicSwapAction::CheckStatus),
                    !complete,
                );
                assert_eq!(
                    swaps
                        .shown_public_swaps()
                        .any(|(shown, _)| shown == identity),
                    !complete,
                );
                // The confirmed read never rewrites the bridge's original fill evidence.
                assert!(matches!(
                    swap.observations().bridge_outcome,
                    Some(SwapBridgeOutcome::HeldOnDestination { amount, block, .. })
                        if amount == U256::from(100) && block.number == 20
                ));
            });
            cx.update(|window, cx| {
                use gpui_kit::test::TestWindowExt;
                window.render_frame(cx);
                assert_eq!(
                    window
                        .try_find("public-swap-action-RecoverDestination")
                        .is_some_and(|button| button.visible()),
                    !complete,
                );
            });
        }
    });
}

#[gpui::test]
fn retrying_an_expired_public_order_starts_a_fresh_review_and_preserves_its_history(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        AcrossOrderTerms, PublicSwapClaim, PublicSwapObservations, SwapAccountChoice,
        SwapObservation,
    };
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    enable_stub_chain(root, &stubs, 1);
                    enable_stub_chain(root, &stubs, 137);
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
            let template =
                model::tests::public_presentation_record(true, &PublicSwapObservations::default());
            let saved = template.swap_uses()[0].public_swap().unwrap();
            let mut approval = saved.approval().clone();
            approval.sell_token = Address::repeat_byte(42);
            approval.bounds.sell_amount = U256::from(7_000_000_000_000_000_000_u64);
            approval.bounds.slippage_bps = 125;
            approval.bounds.gas_share_bps = Some(3_000);
            approval.destination.address = destination.address();
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
                    bridged_token: saved.intent().bridged_token,
                    order: true,
                    approval: approval.clone(),
                    now: now_unix(),
                })
                .unwrap();
            destination_store
                .record_public_swap_path(
                    operation,
                    id,
                    saved.path().unwrap().clone(),
                    AcrossOrderTerms {
                        spoke_pool: Address::repeat_byte(8),
                        input_token: saved.intent().bridged_token,
                        output_token: STUB_POLYGON_USDC,
                        input_amount: U256::from(995),
                        output_amount: U256::from(990),
                        quote_timestamp: 0,
                        fill_deadline: 100,
                        exclusive_relayer: Address::ZERO,
                        exclusivity_parameter: 0,
                        recipient: destination.address(),
                        message_hash: Some(B256::ZERO),
                    },
                )
                .unwrap();
            let old = destination_store
                .record_public_swap_observations(
                    operation,
                    id,
                    PublicSwapObservations {
                        expired: Some(SwapObservation {
                            block: BlockNumHash::new(20, B256::ZERO),
                            transaction_hash: None,
                        }),
                        ..Default::default()
                    },
                )
                .unwrap();
            let identity = model::SwapIdentity {
                operation,
                swap_use: id,
            };
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.public_records = vec![(137, old.clone())];
                    swaps.show_view(SwapDialogView::PublicDetail(identity), window, cx);
                });
                window.draw(cx).clear(cx);
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.update(|window, cx| {
                use gpui_kit::test::TestWindowExt;
                window.click("public-swap-action-Retry", cx);
            });
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            let public = form.public.as_ref().unwrap();
            assert_eq!(public.account.public_account_uuid, account.public_account_uuid);
            assert!(public.operation.is_none() && public.swap_use.is_none(), "the signed use is immutable and cannot own the new review");
            assert!(public.review.is_none(), "the old signature and quote cannot authorize another attempt");
            assert_eq!((form.sell, form.network, form.buy), (approval.sell_token, Some(137), Some(STUB_POLYGON_USDC)));
            assert_eq!(form.amount_input.read(cx).value().as_ref(), "7");
            assert_eq!((form.slippage_bps, form.gas_share_bps), (125, 3_000));
            assert!(form.destination_account.is_none(), "the previously claimed account is checked afresh instead of selected automatically");
            assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
        });
            assert_eq!(
                destination_store
                    .records()
                    .unwrap()
                    .into_iter()
                    .find(|record| record.operation() == operation)
                    .unwrap(),
                old
            );
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}
