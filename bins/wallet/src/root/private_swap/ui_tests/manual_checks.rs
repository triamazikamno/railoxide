use super::*;

/// Check jobs can persist an outcome before a later RPC fails. With the destination still
/// loaded, both that failure and a successful correction must update its outcome. Neither
/// outcome revokes a shield signature at the unchanged nonce. The checked swap reuses both
/// accounts of an earlier, delivered swap, and each swap's delivery stays its own.
#[gpui::test]
fn manual_checks_reconcile_destination_payloads_even_after_a_later_error(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::{
        Call, CommitmentPreimage, RelayAdapt7702, ShieldCiphertext, ShieldRequest, TokenData,
        shieldCall,
    };
    use wallet_ops::ExecutorAsset;
    use wallet_ops::vault::{
        AcrossOrderTerms, BridgeOrderTerms, ExecutorInputIdentity, ExecutorNonceObservation,
        ExecutorNonceWatermark, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, SwapAccountChoice, SwapAttempt, SwapBridgeOutcome,
        SwapDestinationClaim, SwapDestinationOutcome, SwapPairClaim, SwapProof, SwapTerms,
        SwapUseId,
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
                .record_swap_bridge_outcome(operation, earlier_uid, earlier_delivery.clone())
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
                        delivery: delivery.clone(),
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
                        speed_up: None,
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
            // The held USDC counts as recovered only once a recovery of USDC is attributed
            // after the hold and this order's own check reads none left. A recovery of the
            // reused account's earlier USDT, or a check made for the earlier order, says
            // nothing about it.
            let recovered = |token: Address| {
                let record = destination.records().unwrap().remove(0);
                let request = ShieldRequest {
                    preimage: CommitmentPreimage {
                        npk: B256::repeat_byte(0x7a),
                        token: TokenData::erc20(token),
                        value: alloy::primitives::Uint::from(10_u64),
                    },
                    ciphertext: ShieldCiphertext {
                        encryptedBundle: [B256::ZERO; 3],
                        shieldKey: B256::ZERO,
                    },
                };
                let before = ExecutorNonceObservation::new(
                    BlockNumHash::new(41, B256::ZERO),
                    U256::from(10),
                );
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
                    _nonce: U256::from(10),
                    _signature: Bytes::new(),
                }
                .abi_encode();
                let payload = IssuedExecutorPayload::new(
                    U256::from(10),
                    record.delegate(),
                    B256::repeat_byte(0x7a),
                    ExecutorPayloadPurpose::Recovery,
                    ExecutorPayloadContext::new(calldata.into(), before, Vec::new()),
                );
                let mut saved = serde_json::to_value(&record).unwrap();
                saved["issued"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::to_value(payload).unwrap());
                saved["nonce_watermark"] =
                    serde_json::to_value(ExecutorNonceWatermark::new(U256::from(11), 43)).unwrap();
                (
                    serde_json::from_value::<ExecutorRecord>(saved).unwrap(),
                    wallet_ops::ExecutorAttribution::with_shields_for_tests(
                        Address::ZERO,
                        &[(request, 42)],
                    ),
                )
            };
            let read = |order, amount: u64| {
                Some((order, U256::from(amount), BlockNumHash::new(43, B256::ZERO)))
            };
            for (token, balance, complete) in [
                (STUB_POLYGON_USDT, read(uid, 0), false),
                (STUB_POLYGON_USDC, read(uid, 5), false),
                (STUB_POLYGON_USDC, None, false),
                (STUB_POLYGON_USDC, read(earlier_uid, 0), false),
                (STUB_POLYGON_USDC, read(uid, 0), true),
            ] {
                let (account, attribution) = recovered(token);
                cx.update(|_, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.reload_records();
                        swaps.reload_destinations(cx);
                        let shown = swaps.shown_swap(swaps.record(operation).unwrap()).unwrap();
                        swaps.destinations.insert(shown, Some(account));
                        swaps
                            .attributions
                            .insert(destination_operation, attribution);
                        swaps
                            .tracking
                            .entry(operation)
                            .or_default()
                            .destination_balance = balance;
                        let stage = swaps.progress_stage(swaps.record(operation).unwrap());
                        assert_eq!(
                            stage,
                            if complete {
                                SwapStage::Recovered
                            } else {
                                SwapStage::Order(SwapOrderState::HeldOnDestination)
                            },
                            "{token} {balance:?}"
                        );
                        assert_eq!(
                            model::swap_actions(stage, false).recover_on_destination,
                            !complete
                        );
                    });
                });
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
/// recovery, and shows its deposit address in full. Check status asks 1Click again on a fresh
/// route of the swap's own and records the delivery it reports. The swap's next RPC page reads
/// over that route.
#[gpui::test]
fn near_intents_deposit_that_needs_attention_is_checked_with_the_provider(cx: &mut TestAppContext) {
    use wallet_ops::vault::{BridgeDelivery, BridgeProvider, BridgeSurplus, SwapBridgeOutcome};

    let stubs = SwapStubs::start();
    let rpc = Some(stubs.rpc());
    with_approved_swap_view_and_rpc(cx, rpc, |root, swaps, executors, operation, runtime, cx| {
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
            root.update(cx, |root, _| {
                enable_stub_chain(root, &stubs, 137);
                root.http = tagged_operation_http();
            });
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
        let before_check = stubs.request_count();
        cx.simulate_click(check.center(), gpui::Modifiers::none());
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| swaps.job.is_none())
        });
        // Untagged requests come from the stub orderbook's route or the wallet session.
        let tagged = |routes: BTreeSet<Option<String>>| {
            routes.into_iter().flatten().collect::<BTreeSet<_>>()
        };
        let checked = tagged(stubs.routes_since(before_check, "/near/"));
        assert_eq!(
            checked.len(),
            1,
            "the check asks on one fresh route: {checked:?}"
        );
        // The swap's next RPC page goes over the route the check replaced its own with.
        let (owner, page) = cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                let http = swaps.http(cx).unwrap();
                let page = ObservationPage {
                    operation,
                    settlement: None,
                    range: Some(0..1),
                    destination: None,
                    route: swaps.swap_route(&http, operation),
                };
                (Arc::clone(swaps.private_owner().unwrap()), page)
            })
        });
        let before_page = stubs.request_count();
        runtime.block_on(observe_pages(owner, vec![page]));
        assert_eq!(tagged(stubs.routes_since(before_page, "/rpc")), checked);
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
    with_approved_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
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

    with_approved_swap_view(cx, |_, swaps, executors, operation, runtime, cx| {
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
