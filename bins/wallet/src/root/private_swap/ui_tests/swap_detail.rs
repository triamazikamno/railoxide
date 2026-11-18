use super::*;

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

#[gpui::test]
fn place_order_keeps_progress_visible_while_checking_terms_and_after_failure(
    cx: &mut TestAppContext,
) {
    with_approved_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
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
            window.open_wallet_dialog(cx, |dialog, _, _| dialog.title("Unrelated form"));
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

/// The disclaimer opens above each new swap dialog until "I understand" is pressed, and the
/// question-mark button opens it again.
#[gpui::test]
fn swap_disclaimer_opens_until_acknowledged_and_from_its_button(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, _, _, _, cx| {
        root.update(cx, |root, _| {
            root.ui_state.swap_disclaimer_acknowledged = false;
        });
        let open_form = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_new_form(Address::repeat_byte(1), window, cx);
                });
                window.draw(cx).clear(cx);
            });
        };
        let close_swap_dialog = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| swaps.close_swap_dialog(window, cx));
                window.draw(cx).clear(cx);
            });
        };
        let click = |selector: &'static str, cx: &mut gpui::VisualTestContext| {
            let bounds = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.update(|window, cx| window.draw(cx).clear(cx));
        };
        // The disclaimer is shown above the swap dialog, which has the focus otherwise.
        let shown = |cx: &mut gpui::VisualTestContext| {
            let shown = cx.debug_bounds("swap-disclaimer").is_some();
            cx.update(|window, cx| {
                let swaps = swaps.read(cx);
                assert!(swaps.dialog.is_some());
                assert_eq!(swaps.swap_dialog_active(window, cx), !shown);
            });
            shown
        };
        let acknowledged = |cx: &mut gpui::VisualTestContext| {
            root.read_with(cx, |root, _| root.ui_state.swap_disclaimer_acknowledged)
        };
        open_form(cx);
        assert!(shown(cx));
        // Escape closes it without acknowledging, so the next swap dialog shows it again. The
        // focus returns to the swap dialog after the close animation.
        cx.simulate_keystrokes("escape");
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(!shown(cx));
        assert!(!acknowledged(cx));
        close_swap_dialog(cx);
        open_form(cx);
        assert!(shown(cx));
        // "I understand" leaves the swap dialog open, and the disclaimer no longer opens by
        // itself.
        click("swap-disclaimer-acknowledge", cx);
        assert!(!shown(cx));
        assert!(acknowledged(cx));
        close_swap_dialog(cx);
        open_form(cx);
        assert!(!shown(cx));
        click("swap-disclaimer-open", cx);
        assert!(shown(cx));
    });
}
