use super::*;

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

    with_approved_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
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
            speeds_up: false,
        };
        // The original swap's unshielded funds wait in the account. A draft that sells
        // another token doesn't change what its recovery starts with.
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, _| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().pending_order = Some(pending.clone());
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
                swaps.tracking.entry(operation).or_default().pending_order = Some(pending.clone());
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
        // The draft set no account up, so its steps keep the account's and no fact names it.
        assert!(
            cx.debug_bounds(
                format!("submission-step-swap-step-{}-0", operation.opaque_id()).leak()
            )
            .is_some()
        );
        assert!(cx.debug_bounds("swap-detail-account").is_none());

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
                    speed_up: None,
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

/// Closing recovery returns focus to the swap's detail under it, not to the account revealed
/// behind both dialogs.
#[gpui::test]
fn closing_swap_recovery_returns_focus_to_the_swap_detail(cx: &mut TestAppContext) {
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
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

#[gpui::test]
fn expired_order_can_be_checked_and_hidden_without_releasing_inputs(cx: &mut TestAppContext) {
    with_approved_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let (uid, _) = placed_swap(executors, operation, SwapDelivery::Reshield);
        executors
            .record_swap_submission(
                operation,
                uid,
                wallet_ops::vault::SwapSubmissionStatus::Accepted,
            )
            .unwrap();
        let reserved = executors.records().unwrap()[0].reserved_inputs();
        assert_ne!(
            reserved,
            [] as [wallet_ops::vault::ExecutorInputIdentity; 0]
        );
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
                    .is_some_and(|tracking| tracking.check_error.is_some()),
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
            assert!(swaps.tracking[&operation].check_error.is_none());
        });
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| swaps.job.is_none())
        });
        cx.update(|window, cx| {
            assert!(swaps.read(cx).tracking[&operation].check_error.is_some());
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
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
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
                    route: Arc::default(),
                    fee: false,
                    report: Some((
                        CowOrderStatusReport {
                            status: CowOrderStatusHint::Fulfilled,
                            executed_buy_amount: None,
                        },
                        Some(block),
                    )),
                    bridges: Vec::new(),
                    early: None,
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
                swaps
                    .tracking
                    .entry(operation)
                    .or_default()
                    .order_hints
                    .clear();
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
