use super::*;

/// A speed-up whose swap's earlier order approved a lower Across minimum says so before and in
/// its review, asks for acceptance of that delivery for each quote, and binds the accepted
/// minimum into the approval.
#[gpui::test]
fn a_speed_up_review_discloses_and_binds_the_earlier_across_approval(cx: &mut TestAppContext) {
    use broadcaster_core::contracts::cow::OrderUid;
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
            // The form's quote with an earlier order's approval at 90% of its minimum, as the
            // speed-up of an open order that signed a higher minimum.
            let quoted = refreshed_review(swaps, runtime, cx);
            let minimum = quoted.bridge().unwrap().destination_minimum;
            let lower = minimum * U256::from(9) / U256::from(10);
            let review = Arc::new((*quoted).clone().with_continuing_for_tests(vec![
                wallet_ops::SwapContinuingApproval {
                    uid: OrderUid::new(B256::repeat_byte(0x11), Address::repeat_byte(3), 20),
                    attempt: 0,
                    destination_minimum: lower,
                    post_hook_nonce: U256::from(2),
                    quote_timestamp: 1,
                },
            ]));
            assert_eq!(review.continuing_minimum(), Some(lower));
            let signed = review
                .buy_amount_for(review.suggested_private_minimum())
                .unwrap();
            let mut bounds = test_approval().bounds;
            bounds.buy_amount = signed + U256::ONE;
            let open = SpeedUpOrder {
                uid: OrderUid::new(B256::repeat_byte(0x12), Address::repeat_byte(3), 20),
                bounds,
            };
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    // Only the earlier approval is left to accept.
                    let form = swaps.form.as_mut().unwrap();
                    form.speed_up = Some(open.clone());
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.quote = QuoteState::Ready(Arc::clone(&review));
                    cx.notify();
                });
            });

            // The review stays closed until the lower delivery is accepted in the form.
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert!(!form.continuing_acknowledged);
                assert!(form.review_problem(&review).is_some());
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let checkbox = cx.debug_bounds("swap-continuing-acknowledged").unwrap();
            cx.simulate_click(
                gpui::point(checkbox.left() + gpui::px(8.), checkbox.center().y),
                gpui::Modifiers::none(),
            );
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert!(form.continuing_acknowledged);
                assert_eq!(form.review_problem(&review), None);

                // The review names both orders' signed minimums on the swap's own network, and
                // the earlier approval beside this order's minimum on the destination network.
                let amount =
                    |amount| swaps.network_token_amount(137, STUB_POLYGON_USDT, amount, cx);
                let earlier = format!("{} on Polygon", amount(lower));
                let summary = swaps.swap_summary(&review, None, None, None, Some(&open), cx);
                let rows = summary.rows_for_test();
                assert!(
                    rows.contains(&("Earlier approval".to_owned(), earlier.clone())),
                    "{rows:?}"
                );
                for minimum in [open.bounds.buy_amount, signed] {
                    let shown = format!(
                        "{} on {}",
                        swaps.token_amount(review.plan().buy_token(), minimum, cx),
                        swaps.chain_label()
                    );
                    assert!(rows.iter().any(|(_, value)| *value == shown), "{rows:?}");
                }
                assert_eq!(
                    summary.receive_card_for_test().map(|(_, amount, _)| amount),
                    Some(amount(minimum)),
                    "the Receive card keeps this order's minimum"
                );
                let own = amount(minimum);
                assert!(
                    summary
                        .warnings_for_test()
                        .iter()
                        .any(|warning| warning.contains(&earlier) && warning.contains(&own)),
                    "{:?}",
                    summary.warnings_for_test()
                );
            });

            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    // The speed-up's review, as its form opens it.
                    swaps.form.as_mut().unwrap().operation = Some(operation);
                    swaps.request_order_review(None, window, cx);
                    let pending = swaps
                        .pending_authorization
                        .clone()
                        .unwrap_or_else(|| panic!("{:?}", swaps.form.as_ref().unwrap().error));
                    let SwapAction::Order(approval) = &pending.action else {
                        panic!("the speed-up's review");
                    };
                    assert!(Arc::ptr_eq(&approval.review, &review));
                    assert_eq!(approval.speed_up, Some(open.uid));
                    assert_eq!(approval.continuing_minimum, Some(lower));

                    // Another gas share asks again.
                    swaps.set_gas_preset(GasPreset::Loose, window, cx);
                    assert!(
                        !swaps.form.as_ref().unwrap().continuing_acknowledged,
                        "acceptance of the previous quote cannot carry across changed terms"
                    );
                });
            });
        },
    );
}

/// Speed up… sits in the order step as the executor owner's rule answers for the swap's own
/// records: offered for an open order, disabled while a second order can still fill, offered
/// again once the earlier order's expiry is recorded, and gone once the account's nonce is past
/// the pre-hook nonce both orders share. Opening it asks for a quote planned from the open
/// order's record, keeps that order's terms, and shows the form's own state for a failed quote.
#[gpui::test]
fn speed_up_follows_the_owners_rule_for_the_swaps_orders(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        ExecutorNonceObservation, SwapObservation, SwapOrderObservations, SwapSubmissionStatus,
    };
    with_approved_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
        let now = u32::try_from(now_unix()).unwrap();
        let place = |uid_hash, hook_hash, valid_to, speeds_up, buy_amount: u64| {
            let (uid, observed) = placed_order(
                executors,
                operation,
                SwapDelivery::Reshield,
                PlacedOrder {
                    uid_hash,
                    hook_hash,
                    valid_to,
                    speeds_up,
                },
                |bounds| {
                    bounds.buy_amount = U256::from(buy_amount);
                    bounds.private_minimum = U256::from(buy_amount - 1);
                },
            );
            executors
                .record_swap_submission(operation, uid, SwapSubmissionStatus::Accepted)
                .unwrap();
            (uid, observed)
        };
        // The detail as the records now stand: its Speed up button, and the rule's answer.
        let detail = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            let refusal = swaps.read_with(cx, |swaps, _| {
                swaps.speed_up_refusal(swaps.record(operation).unwrap())
            });
            (cx.debug_bounds("swap-progress-speed-up"), refusal)
        };

        let (first, observed) = place(0x11, 7, now + 600, None, 99);
        // The quote the form asks for goes nowhere.
        let unreachable = orderbook_at("http://127.0.0.1:1/mainnet".parse().unwrap(), runtime);
        let (button, refusal) = detail(cx);
        assert_eq!(refusal, None, "one open order can be sped up");
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| {
                swaps.tracking.entry(operation).or_default().orderbook = Some(unreachable);
            });
        });
        let button = button.expect("an open order offers Speed up");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_ref().expect("Speed up opens the swap form");
                assert_eq!(form.speed_up.as_ref().map(|open| open.uid), Some(first));
                assert_eq!(swaps.form_mode(form), FormMode::Order);
                assert!(
                    matches!(form.quote, QuoteState::Loading),
                    "a speed-up opens on a fresh quote"
                );
                let terms = |swaps: &PrivateSwapsView, cx: &App| {
                    let form = swaps.form.as_ref().unwrap();
                    (
                        form.slippage_bps,
                        form.valid_for,
                        form.receive_to,
                        form.amount_input.read(cx).value().to_string(),
                    )
                };
                let fixed = terms(swaps, cx);
                swaps.set_slippage(fixed.0 + 50, window, cx);
                swaps.set_valid_for(fixed.1 + Duration::from_mins(30), window, cx);
                swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                swaps.use_amount(U256::from(7), window, cx);
                assert_eq!(
                    terms(swaps, cx),
                    fixed,
                    "a speed-up keeps its open order's terms"
                );
            });
        });
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| {
                !matches!(
                    swaps.form.as_ref().map(|form| &form.quote),
                    Some(QuoteState::Loading)
                )
            })
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        swaps.read_with(cx, |swaps, _| {
            assert!(
                matches!(swaps.form.as_ref().unwrap().quote, QuoteState::Failed(_)),
                "this wallet holds none of the order's notes"
            );
        });
        assert!(cx.debug_bounds("swap-price-error").is_some());
        assert!(cx.debug_bounds("swap-price-retry").is_some());

        // A second order of the same use, at the same pre-hook nonce: two can still fill.
        let (second, _) = place(0x12, 0x21, now + 900, Some(first), 98);
        let (button, refusal) = detail(cx);
        assert_eq!(refusal, Some(SwapSpeedUpRefusal::TwoOrdersFillable));
        let button = button.expect("Speed up stays in view with its reason");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, _| {
            assert!(swaps.form.is_none(), "a disabled Speed up opens nothing");
        });

        // The earlier order's recorded expiry takes it out of the fillable set.
        let seen = SwapObservation {
            block: observed.block(),
            transaction_hash: None,
        };
        executors
            .record_swap_observations(
                operation,
                first,
                SwapOrderObservations {
                    expired: Some(seen),
                    ..Default::default()
                },
            )
            .unwrap();
        let (button, refusal) = detail(cx);
        assert_eq!(refusal, None, "the remaining open order can be sped up");
        assert!(button.is_some());
        swaps.read_with(cx, |swaps, _| {
            let latest = swaps.record(operation).unwrap().swap().unwrap().orders();
            assert_eq!(latest.last().map(SwapOrderRecord::uid), Some(second));
        });

        // The shared nonce is used, though no order's pre-hook is recorded as executed.
        executors
            .record_account_read(
                operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(31, B256::repeat_byte(31)),
                    U256::from(2),
                ),
            )
            .unwrap();
        let (button, refusal) = detail(cx);
        assert_eq!(refusal, Some(SwapSpeedUpRefusal::Unavailable));
        assert!(
            button.is_none(),
            "an order that can't be sped up offers none"
        );
    });
}

/// Inside the profile's validity window, an earlier order past its own expiry still counts as
/// fillable, so its local expiry alone doesn't offer Speed up again.
#[gpui::test]
fn an_earlier_order_inside_the_validity_window_holds_speed_up_back(cx: &mut TestAppContext) {
    earlier_order_expiry(cx, false);
}

/// Past the profile's validity window, the earlier order no longer counts, and the open order
/// can be sped up.
#[gpui::test]
fn an_earlier_order_past_the_validity_window_frees_speed_up(cx: &mut TestAppContext) {
    earlier_order_expiry(cx, true);
}

/// An open order that sped up an earlier one whose expiry is a minute inside the profile's
/// validity window, or with `past_window` a minute beyond it.
fn earlier_order_expiry(cx: &mut TestAppContext, past_window: bool) {
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let window_secs = swaps.read_with(cx, |swaps, cx| {
            swaps.swap_profile(cx).unwrap().valid_to_window().as_secs()
        });
        let now = now_unix();
        let earlier = if past_window {
            now - window_secs - 60
        } else {
            now - window_secs + 60
        };
        let (first, _) = placed_order(
            executors,
            operation,
            SwapDelivery::Reshield,
            PlacedOrder {
                uid_hash: 0x11,
                hook_hash: 7,
                valid_to: u32::try_from(earlier).unwrap(),
                speeds_up: None,
            },
            |_| {},
        );
        let (second, _) = placed_order(
            executors,
            operation,
            SwapDelivery::Reshield,
            PlacedOrder {
                uid_hash: 0x12,
                hook_hash: 0x21,
                valid_to: u32::try_from(now + 600).unwrap(),
                speeds_up: Some(first),
            },
            |bounds| {
                bounds.buy_amount = U256::from(98);
                bounds.private_minimum = U256::from(97);
            },
        );
        executors
            .record_swap_submission(
                operation,
                second,
                wallet_ops::vault::SwapSubmissionStatus::Accepted,
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let refusal = swaps.read_with(cx, |swaps, _| {
            swaps.speed_up_refusal(swaps.record(operation).unwrap())
        });
        assert_eq!(
            refusal,
            (!past_window).then_some(SwapSpeedUpRefusal::TwoOrdersFillable)
        );
        assert!(
            cx.debug_bounds("swap-progress-speed-up").is_some(),
            "offered, or disabled with its reason"
        );
    });
}

/// Speed up… shows, enabled or disabled, only once the latest order has been signed for 30
/// seconds, and it sits in the order step under the order's ID. An order signed a minute ago
/// offers it there. A speed-up signed just now leaves the detail without the action, and at 30
/// seconds the action is back, disabled while both orders can fill.
#[gpui::test]
fn speed_up_waits_for_the_latest_order_to_be_open_a_while(cx: &mut TestAppContext) {
    use wallet_ops::vault::SwapSubmissionStatus;
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let now = u32::try_from(now_unix()).unwrap();
        // Each order is valid for ten minutes, so it was signed that long before its expiry.
        let place = |uid_hash, hook_hash, valid_to, speeds_up, buy_amount: u64| {
            let (uid, _) = placed_order(
                executors,
                operation,
                SwapDelivery::Reshield,
                PlacedOrder {
                    uid_hash,
                    hook_hash,
                    valid_to,
                    speeds_up,
                },
                |bounds| {
                    bounds.buy_amount = U256::from(buy_amount);
                    bounds.private_minimum = U256::from(buy_amount - 1);
                    bounds.valid_for_secs = Some(600);
                },
            );
            executors
                .record_swap_submission(operation, uid, SwapSubmissionStatus::Accepted)
                .unwrap();
            uid
        };
        let detail = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            cx.debug_bounds("swap-progress-speed-up")
        };

        let first = place(0x11, 7, now + 540, None, 99);
        let button = detail(cx).expect("an order open for a minute offers Speed up");
        let step = cx
            .debug_bounds(format!("submission-step-swap-step-{}-1", operation.opaque_id()).leak())
            .expect("the order step");
        let order_id = cx.debug_bounds("swap-step-order").unwrap();
        assert!(
            step.contains(&button.origin) && step.contains(&button.center()),
            "Speed up is in the order step, not the footer: {button:?} in {step:?}"
        );
        assert!(step.contains(&order_id.origin) && order_id.bottom() <= button.top());

        place(0x12, 0x21, now + 600, Some(first), 98);
        assert!(
            detail(cx).is_none(),
            "a just-placed order shows no Speed up, enabled or disabled"
        );
        swaps.read_with(cx, |swaps, _| {
            let record = swaps.record(operation).unwrap();
            assert_eq!(
                swaps.speed_up_refusal(record),
                Some(SwapSpeedUpRefusal::TwoOrdersFillable)
            );
            let latest = record.swap().unwrap().orders().last().unwrap();
            let open = model::swap_actions(SwapStage::Order(SwapOrderState::Open), false);
            for (elapsed, shown) in [(29, false), (30, true)] {
                let shows = model::speed_up_shown(latest, u64::from(now) + elapsed);
                assert_eq!(shows, shown, "{elapsed} seconds after signing");
                assert_eq!(
                    open.with_speed_up(None, shows).speed_up,
                    shown.then_some(Ok(())),
                    "an eligible order, {elapsed} seconds after signing"
                );
                let held = open
                    .with_speed_up(Some(SwapSpeedUpRefusal::TwoOrdersFillable), shows)
                    .speed_up;
                assert_eq!(
                    (held.is_some(), matches!(held, Some(Err(_)))),
                    (shown, shown),
                    "two fillable orders, {elapsed} seconds after signing"
                );
            }
        });
    });
}

/// A speed-up the orderbook rejected doesn't lead the swap while the accepted original can
/// still fill. The swap stays open and cancellable on the original, both orders stay tracked
/// with the later expiry, and the rejection leads only once the original is recorded dead.
#[gpui::test]
fn a_rejected_speed_up_leaves_the_swap_on_its_open_original(cx: &mut TestAppContext) {
    use wallet_ops::vault::{
        SwapObservation, SwapOrderObservations, SwapPreHookDeath, SwapPreHookDeathCause,
        SwapSubmissionStatus,
    };
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let now = u32::try_from(now_unix()).unwrap();
        let place = |uid_hash, hook_hash, valid_to, speeds_up, minimum: u64| {
            placed_order(
                executors,
                operation,
                SwapDelivery::Reshield,
                PlacedOrder {
                    uid_hash,
                    hook_hash,
                    valid_to,
                    speeds_up,
                },
                |bounds| {
                    bounds.buy_amount = U256::from(minimum + 1);
                    bounds.private_minimum = U256::from(minimum);
                },
            )
        };
        let (original, observed) = place(0x11, 7, now + 600, None, 98);
        executors
            .record_swap_submission(operation, original, SwapSubmissionStatus::Accepted)
            .unwrap();
        let (speed_up, _) = place(0x12, 0x21, now + 900, Some(original), 97);
        let stage = |cx: &mut gpui::VisualTestContext| {
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, _| {
                    swaps.reload_records();
                    swaps.stage(swaps.record(operation).unwrap())
                })
            })
        };
        // A speed-up whose submission is unconfirmed leads, as the latest order does.
        assert_eq!(stage(cx), SwapStage::SubmissionPending);

        executors
            .record_swap_submission(operation, speed_up, SwapSubmissionStatus::Rejected)
            .unwrap();
        let open = stage(cx);
        assert_eq!(open, SwapStage::Order(SwapOrderState::Open));
        assert_eq!(model::swap_actions(open, false).cancel, Some(Ok(())));
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                let record = swaps.record(operation).unwrap();
                assert_eq!(
                    model::tracked_orders(record)
                        .into_iter()
                        .map(wallet_ops::vault::SwapOrderRecord::uid)
                        .collect::<Vec<_>>(),
                    [original, speed_up]
                );
                assert_eq!(
                    swaps
                        .next_order_hints(cx)
                        .map_or_else(Vec::new, |(_, _, requests)| {
                            requests
                                .iter()
                                .map(|request| request.uid)
                                .collect::<Vec<_>>()
                        }),
                    [original, speed_up],
                    "the rejected order is still asked about"
                );
                let latest = record.swap().unwrap().orders().last().unwrap();
                let attempts = model::SwapAttempts::ending_at(record, latest);
                assert_eq!(
                    swaps
                        .attempt_rows(record, &attempts, cx)
                        .iter()
                        .map(|row| (row.label.as_str(), row.order))
                        .collect::<Vec<_>>(),
                    [
                        ("Order 1 · Open", original),
                        ("Order 2 · Rejected", speed_up)
                    ]
                );
                assert_eq!(
                    swaps.labels(record, cx).expires,
                    Some(local_time_label(u64::from(now + 900))),
                    "the rejected order can still fill until its own expiry"
                );
            });
        });

        // With the original dead, no accepted order is left and the rejection leads.
        executors
            .record_swap_observations(
                operation,
                original,
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
        assert_eq!(stage(cx), SwapStage::SubmissionRejected);
    });
}

/// The original order trades and is shielded after its speed-up was signed. Routine polling
/// asks about both orders and reads the block a report locates, and the swap stays one entry
/// that follows the original order. The speed-up reads as not filled, and no later report on
/// it moves the original's settlement or the outcome.
#[gpui::test]
fn an_original_order_that_trades_after_its_speed_up_decides_the_swap(cx: &mut TestAppContext) {
    competing_orders_follow_the_one_that_traded(cx, true);
}

/// The same with the speed-up trading and the original order left unfilled.
#[gpui::test]
fn a_speed_up_that_trades_decides_the_swap(cx: &mut TestAppContext) {
    competing_orders_follow_the_one_that_traded(cx, false);
}

fn competing_orders_follow_the_one_that_traded(cx: &mut TestAppContext, original_wins: bool) {
    use wallet_ops::vault::{
        SwapObservation, SwapOrderObservations, SwapSubmissionStatus, SwapTradeAmounts,
    };
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let now = u32::try_from(now_unix()).unwrap();
        let place = |uid_hash, hook_hash, valid_to, speeds_up, minimum: u64| {
            let (uid, observed) = placed_order(
                executors,
                operation,
                SwapDelivery::Reshield,
                PlacedOrder {
                    uid_hash,
                    hook_hash,
                    valid_to,
                    speeds_up,
                },
                |bounds| {
                    bounds.buy_amount = U256::from(minimum + 1);
                    bounds.private_minimum = U256::from(minimum);
                },
            );
            executors
                .record_swap_submission(operation, uid, SwapSubmissionStatus::Accepted)
                .unwrap();
            (uid, observed)
        };
        let (original, observed) = place(0x11, 7, now + 600, None, 98);
        let (speed_up, _) = place(0x12, 0x21, now + 900, Some(original), 97);
        let (winner, loser, minimum) = if original_wins {
            (original, speed_up, 98_u64)
        } else {
            (speed_up, original, 97)
        };
        let report = |uid, status, block: Option<u64>| HintResult {
            operation,
            uid,
            route: Arc::default(),
            fee: false,
            report: Some((
                CowOrderStatusReport {
                    status,
                    executed_buy_amount: None,
                },
                block,
            )),
            bridges: Vec::new(),
            early: None,
        };
        // The settlement each routine page would read, and the orders the orderbook is asked
        // about.
        let located = |swaps: &PrivateSwapsView, cx: &App| {
            swaps
                .next_observations(cx)
                .map_or_else(Vec::new, |(_, _, pages)| {
                    pages
                        .iter()
                        .map(|page| (page.settlement, page.range.clone()))
                        .collect::<Vec<_>>()
                })
        };
        let asked = |swaps: &PrivateSwapsView, cx: &App| {
            swaps
                .next_order_hints(cx)
                .map_or_else(Vec::new, |(_, _, requests)| {
                    requests
                        .iter()
                        .map(|request| (request.uid, request.hint))
                        .collect::<Vec<_>>()
                })
        };
        // The Orders section's rows, as each order's label and ID.
        let attempt_rows = |swaps: &PrivateSwapsView, cx: &App| {
            let record = swaps.record(operation).unwrap();
            let latest = record.swap().unwrap().orders().last().unwrap();
            let attempts = model::SwapAttempts::ending_at(record, latest);
            swaps
                .attempt_rows(record, &attempts, cx)
                .into_iter()
                .map(|row| (row.label, row.order))
                .collect::<Vec<_>>()
        };
        let confirmed = cx.update(|_, cx| {
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
                let block_only = Some(confirmed..confirmed + 1);
                // Both orders can fill: one open swap that keeps both IDs, with the lower
                // minimum and the later expiry.
                let record = swaps.record(operation).unwrap();
                assert_eq!(swaps.stage(record), SwapStage::Order(SwapOrderState::Open));
                assert_eq!(model::record_swaps(record).len(), 1);
                assert_eq!(swaps.order_rows_for_test(cx).len(), 1);
                assert_eq!(swaps.shown_swaps().count(), 1);
                assert_eq!(
                    attempt_rows(swaps, cx)
                        .into_iter()
                        .map(|(_, uid)| uid)
                        .collect::<Vec<_>>(),
                    [original, speed_up]
                );
                let labels = swaps.labels(record, cx);
                assert_eq!(
                    labels.minimum,
                    Some(swaps.token_amount(Address::repeat_byte(2), U256::from(97), cx))
                );
                assert_eq!(labels.expires, Some(local_time_label(u64::from(now + 900))));
                assert_eq!(
                    model::swap_valid_to(record),
                    Some(u64::from(now + 900)),
                    "the swap can fill until its last order expires"
                );
                // Each is asked about, and nothing is read before a report locates a trade.
                assert_eq!(
                    asked(swaps, cx),
                    [(original, true), (speed_up, true)],
                    "both competing orders are followed"
                );
                assert_eq!(
                    located(swaps, cx),
                    [] as [(Option<(OrderUid, u64)>, Option<std::ops::Range<u64>>); 0]
                );
                swaps.apply_order_hints(
                    vec![
                        report(winner, CowOrderStatusHint::Fulfilled, Some(confirmed)),
                        report(loser, CowOrderStatusHint::Open, None),
                    ],
                    cx,
                );
                assert_eq!(
                    located(swaps, cx),
                    [(Some((winner, confirmed)), block_only.clone())],
                    "the reported trade is read from its block alone"
                );
                // A report on the other order is kept beside it, under that order's own ID.
                swaps.apply_order_hints(
                    vec![report(
                        loser,
                        CowOrderStatusHint::Fulfilled,
                        Some(confirmed - 1),
                    )],
                    cx,
                );
                let pages = located(swaps, cx);
                assert_eq!(pages.len(), 2, "one page for each located order");
                assert!(pages.contains(&(Some((winner, confirmed)), block_only.clone())));
                assert!(pages.contains(&(Some((loser, confirmed - 1)), block_only)));
                confirmed
            })
        });

        // The trade is verified on one order. The swap follows it, and the other order is
        // neither asked about nor read again.
        let seen = SwapObservation {
            block: observed.block(),
            transaction_hash: Some(B256::repeat_byte(40)),
        };
        let traded = SwapOrderObservations {
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
            ..Default::default()
        };
        executors
            .record_swap_observations(operation, winner, traded.clone())
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let block_only = Some(confirmed..confirmed + 1);
                let stage = swaps.stage(swaps.record(operation).unwrap());
                assert_eq!(stage, SwapStage::Order(SwapOrderState::Traded));
                assert_eq!(
                    swaps
                        .shown_swaps()
                        .map(|(_, stage)| stage)
                        .collect::<Vec<_>>(),
                    [stage],
                    "the status card shows one swap, at the traded order's stage"
                );
                assert_eq!(asked(swaps, cx), [(winner, true)]);
                assert_eq!(
                    located(swaps, cx),
                    [(Some((winner, confirmed)), block_only.clone())]
                );
                // Late reports on the order that lost change nothing.
                for (status, block) in [
                    (CowOrderStatusHint::Fulfilled, Some(confirmed - 2)),
                    (CowOrderStatusHint::Expired, None),
                ] {
                    swaps.apply_order_hints(vec![report(loser, status, block)], cx);
                    assert_eq!(
                        located(swaps, cx),
                        [(Some((winner, confirmed)), block_only.clone())],
                        "{status:?}"
                    );
                    assert_eq!(swaps.stage(swaps.record(operation).unwrap()), stage);
                }
            });
        });

        // Delivered: the swap is complete under the traded order's terms.
        executors
            .record_swap_observations(
                operation,
                winner,
                SwapOrderObservations {
                    delivered: Some(seen),
                    shielded: Some(
                        serde_json::from_value(serde_json::json!({
                            "observation": seen, "private_amount": "0x62", "fee": "0x1"
                        }))
                        .unwrap(),
                    ),
                    ..traded
                },
            )
            .unwrap();
        let done = SwapStage::Order(SwapOrderState::Done);
        let expected_rows = if original_wins {
            vec![
                ("Order 1 · Traded".to_owned(), original),
                ("Order 2 · Not filled".to_owned(), speed_up),
            ]
        } else {
            vec![
                ("Order 1 · Not filled".to_owned(), original),
                ("Order 2 · Traded".to_owned(), speed_up),
            ]
        };
        // What every view shows of the completed swap, also after a restart.
        let completed = |swaps: &PrivateSwapsView, cx: &App| {
            let record = swaps.record(operation).unwrap();
            assert_eq!(swaps.stage(record), done);
            assert_eq!(model::record_swaps(record).len(), 1);
            let labels = swaps.labels(record, cx);
            assert_eq!(labels.order, Some(winner));
            assert_eq!(
                labels.minimum,
                Some(swaps.token_amount(Address::repeat_byte(2), U256::from(minimum), cx))
            );
            assert!(labels.received.is_some());
            assert_eq!(
                swaps
                    .order_rows_for_test(cx)
                    .into_iter()
                    .map(|(_, status)| status)
                    .collect::<Vec<_>>(),
                [model::swap_order_status(done, false, &labels)],
                "My orders lists the swap once, with the traded order's outcome"
            );
            assert_eq!(attempt_rows(swaps, cx), expected_rows);
            assert!(!swaps.has_shown_swaps(), "a completed swap leaves the card");
            assert_eq!(asked(swaps, cx), [] as [(OrderUid, bool); 0]);
            assert_eq!(
                located(swaps, cx),
                [] as [(Option<(OrderUid, u64)>, Option<std::ops::Range<u64>>); 0]
            );
        };
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                completed(swaps, cx);
                swaps.apply_order_hints(
                    vec![report(
                        loser,
                        CowOrderStatusHint::Fulfilled,
                        Some(confirmed - 3),
                    )],
                    cx,
                );
                completed(swaps, cx);
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_some());
        for uid in [original, speed_up] {
            assert!(
                cx.debug_bounds(format!("swap-attempt-{}", uid.0).leak())
                    .is_some(),
                "each order keeps its row and its copy control"
            );
        }
        let restarted = cx.update(|window, cx| {
            window.close_all_dialogs(cx);
            root.update(cx, |root, cx| {
                root.clear_private_swaps(cx);
                root.ensure_private_swaps(window, cx);
                root.private_swaps_view().unwrap()
            })
        });
        restarted.read_with(cx, |swaps, cx| completed(swaps, cx));
    });
}
