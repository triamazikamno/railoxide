use super::*;

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
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
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
            assert_eq!(corrected.gas_plan().approval_gas_limits, [] as [u64; 0]);
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
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
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
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
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
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
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
                        crate::root::private_swap::public_progress::PublicSwapAction::Continue,
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
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
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

/// A live sync batch on the destination network keeps its session. A Public swap's
/// preparation that finishes while Polygon syncs goes on to the swap's detail.
#[gpui::test]
fn public_preparation_finishing_while_the_destination_syncs_shows_the_swap(
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
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
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

            // Polygon starts its next sync batch with the session the swap was reviewed with.
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    let Some(ChainUtxoState::Ready {
                        snapshot,
                        session,
                        observer_token,
                        sync_tip,
                        poi_refreshing,
                        ppoi_workflow_status,
                    }) = root.chain_states.remove(&137)
                    else {
                        panic!("ready fixture");
                    };
                    root.chain_states.insert(
                        137,
                        ChainUtxoState::Syncing {
                            snapshot,
                            progress: crate::root::chain_load::ChainSyncProgress::default(),
                            catching_up: true,
                            session,
                            observer_token,
                            sync_tip,
                            poi_refreshing,
                            ppoi_workflow_status,
                        },
                    );
                });
            });

            // The preparation finishes mid-batch. Its result isn't dropped as stale.
            let _ = release.send(());
            runtime.block_on(tokio::task::yield_now());
            cx.run_until_parked();
            assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 1);
            swaps.read_with(cx, |swaps, _| {
                assert!(
                    swaps
                        .dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.view == SwapDialogView::PublicDetail(identity)),
                    "the finished preparation shows its swap"
                );
                assert!(swaps.form.is_none() && swaps.public_job.is_none());
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}
