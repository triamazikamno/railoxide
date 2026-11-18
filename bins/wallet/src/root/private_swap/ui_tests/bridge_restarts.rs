use super::*;

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
                    new_destination.then_some(SwapSetupFeeLimit {
                        token: STUB_POLYGON_USDT,
                        amount: U256::from(50_000),
                    });
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
                                    && account
                                        .outcome(swaps.offers_cancelled_source(operation))
                                        .starts_with("Released.")
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
            // each is admitted again. The source stays hidden, so only the destination is
            // listed.
            for (store, account) in [(executors, operation), (&polygon_store, existing)] {
                let record = record_of(store, account);
                assert_eq!(record.active_swap_use(), None);
                assert!(record.swap_use(first).unwrap().is_stopped());
                assert!(!record.is_retired() && !record.is_swap_setup_stopped());
            }
            assert_eq!(offered(&swaps, cx), (false, true));
            swaps.read_with(cx, |swaps, _| {
                let owner = swaps.private_owner().unwrap();
                assert_eq!(
                    owner
                        .swap_account_refusal(operation, SwapAccountRole::Source)
                        .unwrap(),
                    None
                );
            });

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
                        && destination.outcome(false).starts_with("Unresolved."),
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
            approval.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                token: STUB_USDC,
                amount: U256::from(50_000),
            });
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
            // The setup is sent again from its row. The form still restores the reserved
            // pair for a new review of the swap.
            assert!(cx.debug_bounds("swap-progress-continue").is_none());
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_existing_form(fresh_source, window, cx);
                });
            });
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
                    &operation_client(root, runtime, cx),
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
                wallet_ops::PublicBroadcasterWaku::new(
                    config.build_client().unwrap(),
                    config.to_waku_config(),
                    root.http.clone(),
                    root.submission_scope.clone(),
                )
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.route.estimate = Some(estimate);
                    form.route.candidates = vec![offer];
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
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
