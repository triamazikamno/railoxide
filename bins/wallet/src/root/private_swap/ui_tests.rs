use super::super::*;
use super::*;
use crate::root::chain_load::{ChainUtxoState, WalletSyncLifecycle};
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
                        Some("You allow paying up to $5.90 of gas, 59% of this swap.".to_owned()),
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
        let checkbox = cx.debug_bounds("swap-costs-acknowledged").unwrap();
        assert!(alert.size.width <= gpui::px(360.));
        assert!(
            checkbox.top() >= message.bottom(),
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
        let trigger = cx.debug_bounds("swap-buy-selector").unwrap();
        cx.simulate_click(trigger.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("DAI");
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            assert_eq!(
                swaps.form.as_ref().unwrap().buy_select.read(cx).query(cx),
                "DAI"
            );
        });
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.buy, Some(dai));
                assert_eq!(form.buy_select.read(cx).selected_value(), Some(dai));

                swaps.flip_tokens(window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!((form.sell, form.buy), (dai, Some(usdc)));
                assert_eq!(form.buy_select.read(cx).selected_value(), Some(usdc));

                swaps.set_form_sell(usdc, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(form.buy.is_none());
                assert!(form.buy_select.read(cx).selected_value().is_none());
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
        let owner = swaps.read_with(cx, |swaps, _| Arc::clone(&swaps.owner));
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
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadInclusion,
    };
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let setup = pending_setup(executors, operation);
        // The setup won its nonce, so a new swap can offer this account.
        executors
            .reconcile(
                operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(12, B256::repeat_byte(12)),
                    U256::ONE,
                ),
                &[(
                    setup.issued()[0].hash(),
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(5),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
            )
            .unwrap();
        cx.update(|window, cx| {
            let target = crate::root::stealth_accounts::StealthAccountTarget::new(
                &swaps.read(cx).session,
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
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorInputIdentity, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadPurpose, IssuedExecutorPayload,
    };
    let record = executors
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    executors
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    executors.reconcile(operation, observed, &[]).unwrap();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16197, "commitment": "0x1"
    }))
    .unwrap();
    executors
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                record.delegate(),
                B256::repeat_byte(4),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), observed, vec![input]),
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
            assert_eq!(swap_stage(&record, None, false), SwapStage::SetupPending);
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
fn observation_catchup_continues_successful_pages_but_waits_after_failure(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(400);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let confirmed = swaps.confirmed_block(cx).unwrap();
                let result = |end, outcome| {
                    vec![ObservationResult {
                        operation,
                        range_end: end,
                        outcome,
                    }]
                };
                assert_eq!(
                    swaps.apply_observations(result(164, Ok(None)), window, cx),
                    [operation],
                    "a successful page behind the safe head continues without the polling delay"
                );
                let (_, _, pages) = swaps.next_observations(cx).unwrap();
                assert_eq!(pages[0].range, 164..228);
                assert!(
                    swaps
                        .apply_observations(result(228, Err("RPC unavailable".into())), window, cx,)
                        .is_empty(),
                    "failed reads wait before retrying"
                );
                assert_eq!(
                    swaps.tracking.get(&operation).unwrap().cursor,
                    Some(164),
                    "a failed page must not skip unobserved blocks"
                );
                assert!(
                    swaps
                        .apply_observations(result(confirmed + 1, Ok(None)), window, cx,)
                        .is_empty(),
                    "caught-up reads return to the polling interval"
                );
            });
        });
    });
}

#[gpui::test]
fn pending_setup_can_retry_and_stop_without_losing_its_reservation(cx: &mut TestAppContext) {
    use alloy::eips::{BlockNumHash, eip7702::constants::EIP7702_DELEGATION_DESIGNATOR};
    use alloy::primitives::B256;
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadInclusion,
    };
    with_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let issued = pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, test_approval())
            .unwrap();
        // Hiding an account is only presentation; it must not stop its pending swap.
        executors.set_hidden(operation, true).unwrap();
        let observed = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            swaps
                .next_observations(cx)
                .is_some_and(|(_, _, pages)| pages.iter().any(|page| page.operation == operation))
        };
        // A setup unconfirmed for long is checked every few minutes, not every block.
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
                swaps.tracking.entry(operation).or_default().setup_read_at = Some(Instant::now());
                assert!(!observed(swaps, cx));
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at =
                    Instant::now().checked_sub(DEFERRED_SETUP_OBSERVATION_INTERVAL);
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
                let preview = swaps.owner.swap_setup_preview(operation).unwrap();
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
        let stop = cx.debug_bounds("swap-progress-stop").unwrap();
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
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at = None;
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
                .record_swap_approval(operation, test_approval())
                .is_err()
        );
        let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
        let record = executors
            .reconcile(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::ONE),
                &[(
                    issued.issued()[0].hash(),
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(5),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
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
fn handed_off_setup_waits_for_its_located_inclusion(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadInclusion, ExecutorPayloadPurpose, ExecutorPayloadStatus,
        IssuedExecutorPayload,
    };
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let issued = pending_setup(executors, operation);
        executors
            .record_swap_approval(operation, test_approval())
            .unwrap();
        let observed = |swaps: &PrivateSwapsView, cx: &gpui::App| {
            swaps
                .next_observations(cx)
                .is_some_and(|(_, _, pages)| pages.iter().any(|page| page.operation == operation))
        };
        // Whether a reload counted a newly recorded setup inclusion since the last check.
        let seen = std::cell::Cell::new(0);
        let woke = |swaps: &PrivateSwapsView| {
            let wakes = *swaps.observation_wake.borrow();
            seen.replace(wakes) != wakes
        };
        let history_start = issued.issued()[0].context().history_start();
        let set_head = |head, cx: &mut gpui::App| {
            root.update(cx, |root, _| {
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture");
                };
                sync_tip.head_block = Some(head);
            });
        };
        let depth = cx.update(|_, cx| {
            root.read(cx)
                .effective_chain_configs
                .get(1)
                .unwrap()
                .finality_depth
        });
        // The confirmed block is one short of the setup's history start plus the depth.
        let early_head = history_start + 2 * depth - 1;
        cx.update(|_, cx| {
            set_head(early_head, cx);
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(
                    !woke(swaps),
                    "a setup without an inclusion does not wake polling"
                );
                let tracking = swaps.tracking.entry(operation).or_default();
                tracking.setup = Some(wallet_ops::SwapSetupStatus::Pending);
                tracking.setup_read_at = Some(Instant::now());
                assert!(
                    observed(swaps, cx),
                    "an unlocated recent setup keeps the polling pace"
                );
            });
        });
        let setup = issued.issued()[0].hash();
        executors
            .record_submission(operation, setup, B256::repeat_byte(80))
            .unwrap();
        let located_at =
            |swaps: &PrivateSwapsView| swaps.tracking.get(&operation).unwrap().located_at;
        let expired = || {
            Instant::now()
                .checked_sub(DEFERRED_SETUP_OBSERVATION_INTERVAL)
                .unwrap()
        };
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at = None;
                assert!(
                    !observed(swaps, cx),
                    "no confirmed block can contain a located setup sent after signing yet"
                );
            });
            set_head(100, cx);
            swaps.update(cx, |swaps, cx| {
                let handed_off = located_at(swaps).expect("the hand-off is recorded");
                assert_eq!(handed_off.0, setup);
                assert!(
                    !observed(swaps, cx),
                    "private sync locates a handed-off setup, so even its first read waits"
                );
                swaps.reload_records();
                assert_eq!(
                    located_at(swaps),
                    Some(handed_off),
                    "reloads keep the hand-off time"
                );
                swaps.tracking.get_mut(&operation).unwrap().located_at = Some((setup, expired()));
                assert!(observed(swaps, cx), "a fallback read still happens");
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at = Some(Instant::now());
                assert!(!observed(swaps, cx), "the last read restarts the wait");
                swaps.tracking.get_mut(&operation).unwrap().setup_read_at = Some(expired());
            });
        });
        // A replacement attempt at the same nonce waits again after its own hand-off.
        let retry_observed =
            ExecutorNonceObservation::new(BlockNumHash::new(20, B256::repeat_byte(20)), U256::ZERO);
        executors.reconcile(operation, retry_observed, &[]).unwrap();
        let replacement = B256::repeat_byte(6);
        executors
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    issued.delegate(),
                    replacement,
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"retry"),
                        retry_observed,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        executors
            .record_submission(operation, replacement, B256::repeat_byte(81))
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert_eq!(located_at(swaps).map(|(hash, _)| hash), Some(replacement));
                assert!(
                    !observed(swaps, cx),
                    "the replacement's hand-off defers its first read"
                );
            });
        });
        // An effect-less inclusion still needs the account check that reports it.
        let observed_after = |nonce| {
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), nonce)
        };
        executors
            .reconcile(
                operation,
                observed_after(U256::ONE),
                &[(
                    replacement,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(25, B256::repeat_byte(25)),
                        B256::repeat_byte(81),
                        ExecutorExecutionResult::MissingEffects,
                    ),
                )],
            )
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(woke(swaps), "a newly recorded inclusion wakes polling");
                assert!(
                    observed(swaps, cx),
                    "an effect-less inclusion is checked at once"
                );
            });
        });
        // The earlier attempt won the nonce. Confirmation observation records that from
        // private sync's location without a nonce observation, and the swap is ready to
        // place its approved order without another account read.
        executors
            .reconcile(
                operation,
                observed_after(U256::ONE),
                &[(
                    setup,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(80),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
            )
            .unwrap();
        executors.invalidate_observation(operation).unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert!(woke(swaps), "a newly recorded inclusion wakes polling");
                let record = swaps.record(operation).unwrap();
                assert!(record.nonce_observation().is_none());
                assert!(matches!(
                    record.recorded_payload_status(replacement),
                    Some(ExecutorPayloadStatus::Invalidated { .. })
                ));
                assert_eq!(
                    swaps.tracking.get(&operation).unwrap().setup,
                    Some(wallet_ops::SwapSetupStatus::Pending)
                );
                assert_eq!(swaps.stage(record), SwapStage::Approved);
                assert!(
                    !observed(swaps, cx),
                    "a recorded executed setup needs no setup page"
                );
                swaps.reload_records();
                assert!(
                    !woke(swaps),
                    "an already known inclusion does not wake polling again"
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
        ExecutorExecutionResult, ExecutorInputIdentity, ExecutorNonceObservation,
        ExecutorPayloadContext, ExecutorPayloadInclusion, ExecutorPayloadPurpose,
        IssuedExecutorPayload, SwapAttempt, SwapDelivery, SwapObservation, SwapOrderObservations,
        SwapPreHookDeath, SwapPreHookDeathCause, SwapProof, SwapRecipient, SwapTerms,
    };
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let setup = pending_setup(executors, operation);
        let setup_hash = setup.issued()[0].hash();
        let observed =
            ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
        executors
            .reconcile(
                operation,
                observed,
                &[(
                    setup_hash,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(5),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
            )
            .unwrap();
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
        executors.reconcile(pending, pending_nonce, &[]).unwrap();
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
        },
        price_verified: Some(false),
        price_acknowledged: true,
        delivery: wallet_ops::vault::SwapDelivery::Reshield,
        tokens: None,
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

    test(&root, &swaps, &executors, operation, &runtime, cx);
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
            ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadContext,
            ExecutorPayloadInclusion, ExecutorPayloadPurpose, IssuedExecutorPayload,
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
        executors.reconcile(operation, observed, &[]).unwrap();
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
            .reconcile(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::ONE),
                &[(
                    payload,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(5),
                        ExecutorExecutionResult::Executed,
                    ),
                )],
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
            .record_swap_approval(operation, approval.clone())
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
                        swaps.apply_approved_quote(operation, &approval, result, window, cx);
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
        AcrossOrderTerms, BridgeOrderTerms, BridgeProvider, ExecutorExecutionResult,
        ExecutorInputIdentity, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadInclusion, ExecutorPayloadPurpose, IssuedExecutorPayload,
        NearIntentsOrderTerms, SwapAttempt, SwapDelivery, SwapProof, SwapRecipient, SwapTerms,
    };
    let setup = pending_setup(executors, operation);
    let setup_hash = setup.issued()[0].hash();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
    executors
        .reconcile(
            operation,
            observed,
            &[(
                setup_hash,
                ExecutorPayloadInclusion::new(
                    BlockNumHash::new(11, B256::repeat_byte(11)),
                    B256::repeat_byte(5),
                    ExecutorExecutionResult::Executed,
                ),
            )],
        )
        .unwrap();
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
        ExecutorPayloadPurpose, IssuedExecutorPayload, SwapAttempt, SwapDelivery, SwapObservation,
        SwapOrderObservations, SwapProof, SwapTerms, SwapTradeAmounts,
    };

    with_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        stranded_swap(executors, operation);
        let mut record = executors.records().unwrap().pop().unwrap();
        let previous = record.swap().unwrap().orders().last().unwrap().uid();
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
        let pending = PendingSwapOrder {
            previous_order: Some(previous),
            sell: Address::repeat_byte(2),
            buy: Address::repeat_byte(1),
            delivery: SwapDelivery::Reshield,
            amount: U256::from(50),
            private_minimum: U256::from(45),
            slippage_bps: 100,
            gas_share_bps: wallet_ops::cow::GAS_SHARE_TIGHT_BPS,
            valid_for: Duration::from_mins(30),
            reuse_account: true,
            started_at: now_unix(),
        };
        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
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
                assert_eq!(swaps.past_swap(operation, 0).unwrap().2.uid(), previous);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_none());

        // The previous result remains accessible, but closing and reopening the current
        // swap from Private must keep showing the new submission.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.show_view(dialog::SwapDialogView::PastDetail(operation, 0), window, cx);
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

        // Persist the second order while submission is still in flight. Its UID and durable
        // progress immediately replace the preparation view; the old result stays separate.
        let observed = ExecutorNonceObservation::new(
            BlockNumHash::new(40, B256::repeat_byte(40)),
            U256::from(3),
        );
        let setup = &record.issued()[0];
        executors
            .reconcile(
                operation,
                observed,
                &[(setup.hash(), setup.inclusion().unwrap())],
            )
            .unwrap();
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
        let mut bounds = test_approval().bounds;
        bounds.sell_amount = pending.amount;
        bounds.private_minimum = pending.private_minimum;
        let terms = record.swap().unwrap().terms();
        executors
            .record_swap_attempt(
                operation,
                SwapAttempt {
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
            swaps.update(cx, |swaps, _| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert!(swaps.busy());
                assert!(swaps.pending_order(record).is_none());
                assert_eq!(swaps.progress_stage(record), SwapStage::SubmissionPending);
                assert_eq!(record.swap().unwrap().orders().last().unwrap().uid(), uid);
                assert_eq!(swaps.past_swap(operation, 0).unwrap().2.uid(), previous);
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
                assert!(!pages[0].setup);
                assert!(
                    pages[0].range.end > confirmed,
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
    use wallet_ops::vault::{
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadInclusion,
    };
    let setup = pending_setup(executors, operation);
    executors
        .reconcile(
            operation,
            ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
            &[(
                setup.issued()[0].hash(),
                ExecutorPayloadInclusion::new(
                    BlockNumHash::new(11, B256::repeat_byte(11)),
                    B256::repeat_byte(5),
                    ExecutorExecutionResult::Executed,
                ),
            )],
        )
        .unwrap();
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
        let offers_native = |swaps: &PrivateSwapsView, window: &mut Window, cx: &mut App| {
            let select = swaps.form.as_ref().unwrap().buy_select.clone();
            select.update(cx, |select, cx| {
                select.set_selected_values(&[Address::ZERO], window, cx);
                select.selected_value() == Some(Address::ZERO)
            })
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
                    .owner
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
                assert!(!offers_native(swaps, window, cx));
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
                assert!(
                    !offers_native(swaps, window, cx),
                    "the Buy list stays ERC-20"
                );
                let select = swaps.form.as_ref().unwrap().buy_select.clone();
                select.update(cx, |select, cx| {
                    select.set_selected_values(&[weth], window, cx);
                });
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
                    .owner
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
                swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
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
                    .owner
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
                assert_eq!(form.buy_select.read(cx).selected_value(), weth);
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
                .buy_select_items(form, cx)
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
                swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
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
/// and its Retry brings back both lists without the warning.
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
                    form.bridge.routes_task = None;
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
                swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
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
            assert!(swaps.buy_select_items(form, cx).is_empty());
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
            swaps.update(cx, |swaps, cx| swaps.set_form_network(42161, window, cx));
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
                .buy_select_items(form, cx)
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
                swaps.set_form_buy(Address::ZERO, window, cx);
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
                    swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
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
                    swaps.set_form_buy(Address::ZERO, window, cx);
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

/// The bridge fee counts toward the costs the user authorizes: one that takes them past 20%
/// raises the warning, and the review waits for Swap anyway.
#[gpui::test]
fn high_bridge_fee_requires_swap_anyway(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    stubs.set_across_fee_bps(2_000);
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
            });
        });
        let review = ready_review(swaps, runtime, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-high-costs").is_some());
        let bps = authorized_high_cost(&review).expect("the bridge fee takes the costs past 20%");
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                let message = swaps.authorized_cost_message(&review, bps, cx);
                assert!(message.contains("bridge fee"), "{message}");
                assert!(
                    swaps
                        .swap_summary(&review, None, None, None, cx)
                        .warnings_for_test()
                        .contains(&message)
                );
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
    });
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
                swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
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
                swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
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
                swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
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
                lines.last().map(String::as_str),
                Some("to your private balance")
            );
            assert_eq!(summary.receiver_for_test(), None);
            assert_eq!(summary.rows_for_test()[0], gas_row(swaps, &review, cx));
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
        });
        let (uid, observed) = placed_swap(executors, operation, delivery);
        let handed_off = |outcome| bridge_observations(observed, Some(U256::from(7)), outcome);
        let verified = SwapBridgeOutcome::DeliveredVerified {
            block: BlockNumHash::new(71_904_233, B256::repeat_byte(60)),
            transaction_hash: B256::repeat_byte(61),
            output_amount: BRIDGE_MINIMUM,
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
    use wallet_ops::vault::{
        BridgeDelivery, BridgeProvider, BridgeSurplus, ExecutorExecutionResult,
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadInclusion,
        ExecutorPayloadPurpose, IssuedExecutorPayload, SwapBridgeOutcome, SwapObservation,
        SwapOrderObservations,
    };

    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver: Address::repeat_byte(0x51),
            destination_token: STUB_POLYGON_USDC,
            surplus: BridgeSurplus::KeepInAccount,
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
        let mut won = vec![(setup.hash(), setup.inclusion().unwrap())];
        let at = |number: u64| {
            BlockNumHash::new(number, B256::repeat_byte(u8::try_from(number).unwrap()))
        };
        // A recovery shield at `nonce` confirmed in block `number`, and the account reconciled
        // past it.
        let mut recover = |nonce: u64, number: u64| {
            let before = ExecutorNonceObservation::new(at(number - 1), U256::from(nonce));
            executors.reconcile(operation, before, &won).unwrap();
            let hash = B256::repeat_byte(0x70 + u8::try_from(nonce).unwrap());
            executors
                .record_issued(
                    operation,
                    IssuedExecutorPayload::new(
                        U256::from(nonce),
                        setup.delegate(),
                        hash,
                        ExecutorPayloadPurpose::Recovery,
                        ExecutorPayloadContext::new(
                            Bytes::from_static(b"recover"),
                            before,
                            Vec::new(),
                        ),
                    ),
                )
                .unwrap();
            won.push((
                hash,
                ExecutorPayloadInclusion::new(
                    at(number),
                    B256::repeat_byte(0x80 + u8::try_from(nonce).unwrap()),
                    ExecutorExecutionResult::Executed,
                ),
            ));
            executors
                .reconcile(
                    operation,
                    ExecutorNonceObservation::new(at(number + 1), U256::from(nonce + 1)),
                    &won,
                )
                .unwrap();
        };
        let handed_off = |outcome, bridge_refund| SwapOrderObservations {
            bridge_refund,
            ..bridge_observations(observed, Some(U256::from(7)), outcome)
        };
        let stage = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, _| {
                    swaps.reload_records();
                    swaps.stage(swaps.record(operation).unwrap())
                })
            })
        };
        executors
            .record_swap_observations(operation, uid, handed_off(None, None))
            .unwrap();
        // The surplus is recovered while the deposit is bridging.
        recover(3, 40);
        assert_eq!(stage(cx), SwapStage::Order(SwapOrderState::Bridging));
        let refunding = Some(SwapBridgeOutcome::Refunding);
        executors
            .record_swap_observations(operation, uid, handed_off(refunding, None))
            .unwrap();
        assert_eq!(stage(cx), SwapStage::Order(SwapOrderState::Refunding));
        // The refund arrives after that recovery.
        let refund = SwapObservation {
            block: at(41),
            transaction_hash: Some(B256::repeat_byte(0x90)),
        };
        executors
            .record_swap_observations(operation, uid, handed_off(refunding, Some(refund)))
            .unwrap();
        assert_eq!(stage(cx), SwapStage::Order(SwapOrderState::Refunding));
        recover(4, 55);
        assert_eq!(stage(cx), SwapStage::Recovered);
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
    across_fee_bps: Arc<std::sync::atomic::AtomicU64>,
    across_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    failing: Arc<std::sync::Mutex<Vec<&'static str>>>,
    gas_price_wei: Arc<std::sync::atomic::AtomicU64>,
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
    across_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    failing: Arc<std::sync::Mutex<Vec<&'static str>>>,
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
        let across_fee_bps = Arc::new(std::sync::atomic::AtomicU64::new(10));
        let across_delay_ms = Arc::<std::sync::atomic::AtomicU64>::default();
        let failing = Arc::<std::sync::Mutex<Vec<&'static str>>>::default();
        let gas_price_wei = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let served_gas_price = Arc::clone(&gas_price_wei);
        let fee_amount = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let served_fee_amount = Arc::clone(&fee_amount);
        let recorded = Arc::clone(&quotes);
        let recorded_bridge = Arc::clone(&bridge_requests);
        let recorded_orders = Arc::clone(&order_requests);
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
            across_delay_ms: Arc::clone(&across_delay_ms),
            failing: Arc::clone(&failing),
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
            across_fee_bps,
            across_delay_ms,
            failing,
            gas_price_wei,
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

/// WETH on Ethereum and on Arbitrum One, where it is the wrapped native token.
const STUB_WETH: Address = alloy::primitives::address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
const STUB_ARBITRUM_WETH: Address =
    alloy::primitives::address!("82af49447d8a07e3bd95bd0d56f35241523fbab1");

/// The stub providers' lists from Ethereum. Across bridges USDC and USDT to Polygon, and WETH
/// to Arbitrum One. 1Click lists both stablecoins on Ethereum and Polygon, native POL on
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

/// A new swap of 1 USDC, planned from a 10 USDC stub note, to `Address::repeat_byte(4)` on
/// Polygon through the stub providers, once their routes are listed. Polygon is enabled with
/// the stub's RPC.
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
                .owner
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
            swaps.set_form_network(137, window, cx);
        });
    });
    drive_until(cx, runtime, |cx| {
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            form.bridge.routes.contains_key(&(STUB_USDC, 137))
        })
    });
}

/// Enable the built-in `chain_id` with the stub's RPC.
fn enable_stub_chain(root: &mut WalletRoot, stubs: &SwapStubs, chain_id: u64) {
    let mut enabled = wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(chain_id)
    .unwrap()
    .clone();
    enabled.enabled = true;
    enabled.rpc_route = wallet_ops::RpcChainRoute::new(chain_id, vec![stubs.rpc()]);
    root.effective_chain_configs = root
        .effective_chain_configs
        .clone()
        .into_values()
        .filter(|chain| chain.chain_id != chain_id)
        .chain(std::iter::once(enabled))
        .collect();
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
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadInclusion, ExecutorPayloadPurpose, IssuedExecutorPayload,
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
    executors.reconcile(operation, observed, &[]).unwrap();
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
        .reconcile(
            operation,
            ExecutorNonceObservation::new(confirmed, U256::ONE),
            &[(
                payload,
                ExecutorPayloadInclusion::new(
                    BlockNumHash::new(11, B256::repeat_byte(11)),
                    B256::repeat_byte(5),
                    ExecutorExecutionResult::Executed,
                ),
            )],
        )
        .unwrap();
    executors.record_swap_approval(operation, approval).unwrap();
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
                .owner
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
                .owner
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
