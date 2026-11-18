use super::*;

/// The speed-up form of an open order, opened from its detail and held on a stub quote of 1
/// USDC for USDT. That quote stands in for the speed-up's own, which is planned from the
/// order's notes, and no test wallet holds them. The open order's signed minimum is the
/// quote's at Balanced, so Balanced doesn't lower it and a larger gas share does. Returns the
/// open order's ID and the quote.
fn speed_up_form_on_a_quote(
    root: &Entity<WalletRoot>,
    swaps: &Entity<PrivateSwapsView>,
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) -> (OrderUid, Arc<SwapReview>) {
    open_private_form(root, swaps, stubs, runtime, cx);
    let quoted = ready_review(swaps, runtime, cx);
    let signed = quoted
        .buy_amount_for(quoted.suggested_private_minimum())
        .unwrap();
    let (uid, _) = placed_order(
        executors,
        operation,
        SwapDelivery::Reshield,
        PlacedOrder {
            uid_hash: 0x11,
            hook_hash: 7,
            valid_to: u32::try_from(now_unix() + 600).unwrap(),
            speeds_up: None,
        },
        |bounds| {
            bounds.buy_amount = signed;
            bounds.private_minimum = quoted.suggested_private_minimum();
            bounds.gas_share_bps = Some(GAS_SHARE_BALANCED_BPS);
        },
    );
    executors
        .record_swap_submission(
            operation,
            uid,
            wallet_ops::vault::SwapSubmissionStatus::Accepted,
        )
        .unwrap();
    let orderbook = stub_orderbook(stubs, runtime);
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            swaps.reload_records();
            swaps.show_detail(operation, window, cx);
        });
        window.draw(cx).clear(cx);
    });
    let speed_up = cx.debug_bounds("swap-progress-speed-up").unwrap();
    cx.simulate_click(speed_up.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            let form = swaps.form.as_mut().expect("Speed up opens the swap form");
            assert_eq!(form.speed_up.as_ref().map(|open| open.uid), Some(uid));
            // The request for the order's own quote is dropped with its task.
            form.quote_task = None;
            form.quote_revision = form.quote_revision.wrapping_add(1);
            form.orderbook = Some(orderbook);
            form.quote = QuoteState::Ready(Arc::clone(&quoted));
            form.price_acknowledged = true;
            swaps.sync_gas_controls(window, cx);
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    (uid, quoted)
}

/// A speed-up's form changes only the Minimum: a preset, the bar and a typed minimum each move
/// the gas share, the allowed gas and the minimum together. While the signed minimum isn't
/// below the open order's, the form says so and can't be reviewed. A lower one opens a review
/// that names both signed minimums and is always explicit, also for a summary that a
/// remembered spend authorization could otherwise approve unseen.
#[gpui::test]
fn a_speed_up_form_reviews_only_a_lower_signed_minimum(cx: &mut TestAppContext) {
    use crate::root::spend_authorization::spend_authorization_can_use_cached_password;
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, cx| {
            let (uid, quoted) =
                speed_up_form_on_a_quote(root, swaps, executors, operation, &stubs, runtime, cx);
            let open_minimum = quoted
                .buy_amount_for(quoted.suggested_private_minimum())
                .unwrap();
            // The form's quote at its share, whether that lowers the open order's signed
            // minimum, and what keeps Review… off.
            let shown = |cx: &mut gpui::VisualTestContext| {
                cx.update(|window, cx| window.draw(cx).clear(cx));
                swaps.read_with(cx, |swaps, _| {
                    let form = swaps.form.as_ref().unwrap();
                    let QuoteState::Ready(review) = &form.quote else {
                        panic!("the speed-up's quote");
                    };
                    (
                        Arc::clone(review),
                        form.speed_up_relaxes(review),
                        form.review_problem(review),
                    )
                })
            };

            // Balanced signs the open order's own minimum.
            let (balanced, lowers, problem) = shown(cx);
            assert_eq!(lowers, Some(false));
            assert_eq!(problem, Some(SPEED_UP_NOT_LOWER.into()));
            assert!(cx.debug_bounds("swap-speed-up-minimums").is_some());
            assert!(cx.debug_bounds("swap-speed-up-not-lower").is_some());

            // A preset.
            let loose = cx.debug_bounds("swap-gas-loose").unwrap();
            cx.simulate_click(loose.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            let (review, lowers, _) = shown(cx);
            assert_eq!(lowers, Some(true));
            assert_eq!(review.gas_share_bps(), GAS_SHARE_LOOSE_BPS);
            assert!(
                review.gas_allowance() > balanced.gas_allowance()
                    && review.suggested_private_minimum() < balanced.suggested_private_minimum(),
                "the allowed gas and the minimum follow the share together"
            );
            assert!(cx.debug_bounds("swap-speed-up-minimums").is_some());
            assert!(cx.debug_bounds("swap-speed-up-not-lower").is_none());

            // The bar.
            move_gas_slider(swaps, 50., false, cx);
            let (review, lowers, _) = shown(cx);
            assert_eq!((review.gas_share_bps(), lowers), (5_000, Some(true)));

            // A typed minimum: above the open order's, then below it.
            let type_minimum = |share_bps: u16, cx: &mut gpui::VisualTestContext| {
                let typed = quoted
                    .with_gas_share(share_bps)
                    .unwrap()
                    .suggested_private_minimum();
                let edit = cx.debug_bounds("swap-gas-edit-minimum").unwrap();
                cx.simulate_click(edit.center(), gpui::Modifiers::none());
                cx.run_until_parked();
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
            };
            type_minimum(2_000, cx);
            let (review, lowers, problem) = shown(cx);
            assert!(review.gas_allowance() < balanced.gas_allowance());
            assert_eq!(lowers, Some(false));
            assert_eq!(problem, Some(SPEED_UP_NOT_LOWER.into()));
            assert!(cx.debug_bounds("swap-speed-up-not-lower").is_some());
            type_minimum(3_000, cx);
            let (review, lowers, _) = shown(cx);
            assert!(review.gas_allowance() > balanced.gas_allowance());
            assert_eq!(lowers, Some(true));
            assert!(cx.debug_bounds("swap-speed-up-not-lower").is_none());

            // With its costs accepted, the lower minimum can be reviewed.
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    cx.notify();
                });
            });
            let (review, _, problem) = shown(cx);
            assert_eq!(problem, None);
            let signed = review
                .buy_amount_for(review.suggested_private_minimum())
                .unwrap();
            assert!(signed < open_minimum);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    // The review names both orders' signed minimums. An ordinary review of
                    // the same quote has no row for the open order's.
                    let buy = review.plan().buy_token();
                    let open = swaps.form.as_ref().unwrap().speed_up.clone();
                    let rows = |open: Option<&SpeedUpOrder>| {
                        swaps
                            .swap_summary(&review, None, None, None, open, cx)
                            .rows_for_test()
                    };
                    let (sped_up, ordinary) = (rows(open.as_ref()), rows(None));
                    let amount = |amount| swaps.token_amount(buy, amount, cx);
                    for minimum in [open_minimum, signed] {
                        assert!(
                            sped_up.iter().any(|(_, value)| *value == amount(minimum)),
                            "{sped_up:?}"
                        );
                    }
                    assert!(
                        !ordinary
                            .iter()
                            .any(|(_, value)| *value == amount(open_minimum)),
                        "{ordinary:?}"
                    );

                    swaps.form_primary(window, cx);
                    let pending = swaps
                        .pending_authorization
                        .clone()
                        .unwrap_or_else(|| panic!("{:?}", swaps.form.as_ref().unwrap().error));
                    let SwapAction::Order(approval) = &pending.action else {
                        panic!("the speed-up's review");
                    };
                    assert!(Arc::ptr_eq(&approval.review, &review));
                    assert_eq!(approval.speed_up, Some(uid));
                    assert!(approval.full_review);

                    // The confirm-only summary is the one a remembered authorization approves
                    // without a prompt. A speed-up can't be authorized with it.
                    let confirm_only = swaps.place_summary(approval, cx).unwrap();
                    assert!(spend_authorization_can_use_cached_password(&confirm_only));
                    assert!(!spend_authorization_can_use_cached_password(
                        &authorization_summary(&pending.action, confirm_only.clone())
                    ));
                    let ordinary = SwapAction::Order(Box::new(OrderApproval {
                        speed_up: None,
                        ..(**approval).clone()
                    }));
                    assert!(spend_authorization_can_use_cached_password(
                        &authorization_summary(&ordinary, confirm_only)
                    ));
                });
            });
        },
    );
}

/// A speed-up is signed only for the review that is still pending in the wallet it was opened
/// in. Closing the review returns to the speed-up's form, and its late result signs nothing.
/// Neither does the result of an earlier review once a newer one is pending, nor one that
/// arrives after the wallet's swaps were cleared. The pending review's own result goes on to
/// signing, which here refuses the stand-in quote.
#[gpui::test]
fn a_late_speed_up_authorization_signs_nothing(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, operation, runtime, cx| {
            let (uid, _) =
                speed_up_form_on_a_quote(root, swaps, executors, operation, &stubs, runtime, cx);
            // Loose lowers the open order's minimum, so the speed-up can be reviewed.
            let review = cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.set_gas_preset(GasPreset::Loose, window, cx);
                    match &swaps.form.as_ref().unwrap().quote {
                        QuoteState::Ready(review) => Arc::clone(review),
                        _ => panic!("the quote at Loose"),
                    }
                })
            });
            // Open the review of the form's quote, which a background pass may have replaced.
            let request = |cx: &mut gpui::VisualTestContext| {
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.pending_authorization = None;
                        let form = swaps.form.as_mut().expect("the speed-up's form");
                        form.quote_task = None;
                        form.quote_revision = form.quote_revision.wrapping_add(1);
                        form.quote = QuoteState::Ready(Arc::clone(&review));
                        form.price_acknowledged = true;
                        form.high_costs_acknowledged = true;
                        swaps.form_primary(window, cx);
                        let pending = swaps
                            .pending_authorization
                            .clone()
                            .unwrap_or_else(|| panic!("{:?}", swaps.form.as_ref().unwrap().error));
                        let SwapAction::Order(approval) = &pending.action else {
                            panic!("the speed-up's review");
                        };
                        assert_eq!(approval.speed_up, Some(uid));
                        pending
                    })
                })
            };
            // What signing would leave behind: a job, a draft order, or its refusal on the form.
            let signing = |cx: &mut gpui::VisualTestContext| {
                swaps.read_with(cx, |swaps, _| {
                    (
                        swaps.job.is_some(),
                        swaps
                            .tracking
                            .get(&operation)
                            .is_some_and(|tracking| tracking.pending_order.is_some()),
                        swaps.form.as_ref().and_then(|form| form.error.clone()),
                    )
                })
            };
            // A validation result for `command` reaches the swap view. `true` when it left
            // anything of signing behind.
            let arrive = |command: &Arc<SwapAuthorization>, cx: &mut gpui::VisualTestContext| {
                let before = signing(cx);
                let authorization = DesktopPrivateSpendAuthorization::VaultPassword(
                    zeroize::Zeroizing::new("late".to_owned()),
                );
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.continue_authorized(command, authorization, None, window, cx);
                    });
                });
                cx.run_until_parked();
                signing(cx) != before
            };

            // Escape closes the review, which cancels it and returns to the form.
            let closed = request(cx);
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_keystrokes("escape");
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                let swaps = swaps.read(cx);
                assert!(
                    swaps.pending_authorization.is_none(),
                    "closing the review cancels it"
                );
                assert!(
                    swaps.swap_dialog_shows(operation, window, cx),
                    "focus returns to the speed-up's form"
                );
            });
            assert!(!arrive(&closed, cx), "a closed review signs nothing");

            let _newer = request(cx);
            assert!(
                !arrive(&closed, cx),
                "an earlier review's result can't approve a newer one"
            );

            let pending = request(cx);
            assert!(
                arrive(&pending, cx),
                "the pending review goes on to signing"
            );
            swaps.read_with(cx, |swaps, _| {
                assert!(
                    swaps.job.is_none(),
                    "the stand-in quote is refused unsigned"
                );
            });

            // Switching wallets clears the view the review was opened in.
            let left = request(cx);
            root.update(cx, crate::root::WalletRoot::clear_private_swaps);
            assert!(
                !arrive(&left, cx),
                "a cleared wallet's review signs nothing"
            );
        },
    );
}
