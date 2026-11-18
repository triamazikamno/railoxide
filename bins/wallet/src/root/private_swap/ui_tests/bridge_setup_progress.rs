use super::*;

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
            approval.bounds.destination_setup_fee = Some(SwapSetupFeeLimit {
                token: STUB_POLYGON_USDT,
                amount: U256::from(50_000),
            });
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
                delivery: approval.delivery.clone(),
                amount: approval.bounds.spend_amount(),
                private_minimum: approval.bounds.private_minimum,
                slippage_bps: review.slippage_bps(),
                gas_share_bps: review.gas_share_bps(),
                valid_for: review.valid_for(),
                reuse_account: true,
                swap_use,
                started_at: now_unix(),
                speeds_up: false,
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
                    swaps.tracking.entry(operation).or_default().pending_order =
                        Some(draft.clone());
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
                    tracking.pending_order = Some(draft.clone());
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
            // Only the new account has a setup to show.
            assert!(cx.debug_bounds("swap-setup-column-destination").is_some());
            assert!(cx.debug_bounds("swap-setup-column-origin").is_none());
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
                            ..draft.clone()
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

/// A private Bridge swap shows one setup for each new account that isn't confirmed, the swap's
/// own network first: side by side in a wider dialog, and one above the other in a window too
/// narrow for that. A confirmed account has none, whatever became of the other's setup.
#[gpui::test]
fn private_bridge_setups_show_side_by_side_until_each_is_confirmed(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        ExecutorNonceObservation, SwapApprovalTokens, SwapDestinationClaim, SwapPairClaim,
    };
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, _, runtime, store, cx| {
            // The fixture's root has Polygon only once it is enabled.
            cx.update(|_, cx| {
                root.update(cx, |root, _| enable_stub_chain(root, &stubs, 137));
            });
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let (polygon_store, _) = reusable_polygon_account(root, cx);
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
            // Both accounts are new, and each setup was sent.
            let source = ExecutorOperationId::random().unwrap();
            let destination = ExecutorOperationId::random().unwrap();
            let receiver = Address::repeat_byte(0x61);
            let approval = SwapApproval {
                delivery: SwapDelivery::Bridge(BridgeDelivery {
                    provider: BridgeProvider::Across,
                    destination_chain: 137,
                    receiver,
                    destination_token: STUB_POLYGON_USDT,
                    surplus: BridgeSurplus::Reshield,
                    private: Some(BridgePrivateDelivery {
                        on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
                    }),
                }),
                tokens: Some(SwapApprovalTokens {
                    sell: Address::repeat_byte(1),
                    buy: Address::repeat_byte(2),
                }),
                accounts: Some(SwapApprovedAccounts {
                    source: SwapApprovedAccount {
                        address: None,
                        setup: true,
                    },
                    destination: Some(SwapApprovedAccount {
                        address: None,
                        setup: true,
                    }),
                }),
                ..test_approval()
            };
            executors
                .claim_swap_pair(SwapPairClaim {
                    id: SwapUseId::first(source),
                    source: SwapAccountChoice::New(source),
                    delegate: origin_delegate,
                    purpose_summary: Some("Private swap".into()),
                    assets: Vec::new(),
                    approval,
                    destination: Some(SwapDestinationClaim {
                        chain_id: 137,
                        account: SwapAccountChoice::New(destination),
                        delegate: polygon_delegate,
                        destination_token: STUB_POLYGON_USDT,
                    }),
                })
                .unwrap();
            executors
                .bind_address(source, Address::repeat_byte(0x33))
                .unwrap();
            polygon_store.bind_address(destination, receiver).unwrap();
            record_setup(
                executors,
                source,
                origin_delegate,
                B256::repeat_byte(0x43),
                Vec::new(),
            );
            record_setup(
                &polygon_store,
                destination,
                polygon_delegate,
                B256::repeat_byte(0x44),
                Vec::new(),
            );
            let show = |cx: &mut gpui::VisualTestContext, width: f32| {
                cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(1400.)));
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.reload_records();
                        swaps.reload_destinations(cx);
                        swaps.show_detail(source, window, cx);
                    });
                    window.refresh();
                    window.draw(cx).clear(cx);
                });
            };
            let columns = |cx: &mut gpui::VisualTestContext| {
                (
                    cx.debug_bounds("swap-setup-column-origin"),
                    cx.debug_bounds("swap-setup-column-destination"),
                )
            };

            show(cx, 1000.);
            let (Some(origin), Some(destination_column)) = columns(cx) else {
                panic!("both setups show");
            };
            assert!(
                origin.origin.x < destination_column.origin.x
                    && origin.origin.y == destination_column.origin.y,
                "side by side, the swap's own network first"
            );
            show(cx, 700.);
            let (Some(origin), Some(destination_column)) = columns(cx) else {
                panic!("both setups show");
            };
            assert!(
                origin.origin.x == destination_column.origin.x
                    && origin.origin.y < destination_column.origin.y,
                "stacked in the same order"
            );

            // The swap's own setup is confirmed, and a broadcaster refused the other.
            confirm_setup(
                executors,
                source,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(12, B256::repeat_byte(12)),
                    U256::ONE,
                ),
            );
            swaps.update(cx, |swaps, cx| {
                drop(swaps.start_setup_progress(
                    source,
                    SetupSide::Destination,
                    setup_broadcaster(),
                    cx,
                ));
                swaps.finish_setup_progress(
                    source,
                    SetupSide::Destination,
                    Some(Ok(&PublicBroadcasterResultKind::Failed {
                        error: "fee too low".into(),
                    })),
                );
            });
            show(cx, 1000.);
            let (origin, destination_column) = columns(cx);
            assert!(origin.is_none() && destination_column.is_some());
            let refused: &'static str = format!(
                "submission-step-swap-setup-{}-destination-wait-response",
                source.opaque_id()
            )
            .leak();
            assert!(cx.debug_bounds(refused).is_some());
            // The confirmed setup isn't sent again, and doesn't hold the other one's retry back.
            swaps.read_with(cx, |swaps, cx| {
                let record = swaps.record(source).unwrap();
                assert_eq!(
                    swaps.setup_retry_availability(record, SetupSide::Origin, cx),
                    None
                );
                assert_eq!(
                    swaps.setup_retry_availability(record, SetupSide::Destination, cx),
                    Some(Ok(()))
                );
            });

            // Polygon syncs again. The retry's refusal shows under the detail, beside the
            // broadcaster's problem that the destination setup's own stage keeps.
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
            // A synced network's catch-up doesn't read as syncing to the swap form, which would
            // drop its quote.
            swaps.read_with(cx, |swaps, cx| {
                let availability = swaps.network_availability(ReceiveTo::PrivateBalance, 137, cx);
                assert!(
                    availability.is_some_and(|it| !matches!(it, NetworkAvailability::Syncing(_))),
                    "{availability:?}"
                );
            });
            show(cx, 1000.);
            assert!(cx.debug_bounds("swap-progress-error").is_none());
            let retry = cx.debug_bounds("swap-setup-retry-destination").unwrap();
            cx.simulate_click(retry.center(), gpui::Modifiers::none());
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
            swaps.read_with(cx, |swaps, _| {
                assert!(swaps.setup_retry.is_none());
                let error = swaps.tracking[&source].error.as_deref();
                assert!(error.is_some_and(|error| error.contains("is still syncing")));
            });
            assert!(cx.debug_bounds("swap-progress-error").is_some());
            assert!(cx.debug_bounds(refused).is_some());
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// Approving the retry of a private Bridge swap's destination setup starts that account's
/// stages again. The swap's own setup keeps the stages it ended with and what this session
/// observed of it. A retry that pays in another fee token than the destination's approved
/// limit saves its token and limit for that account, and the source's limit stays.
#[gpui::test]
fn retrying_the_destination_setup_keeps_the_source_setups_progress(cx: &mut TestAppContext) {
    use crate::root::private_swap::setup_progress::{SetupProgress, SetupProgressEnd};
    use wallet_ops::vault::{SwapApprovalTokens, SwapDestinationClaim, SwapPairClaim};
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, _, runtime, store, cx| {
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
            let polygon_owner = polygon.executor_owner().unwrap();
            let (polygon_store, _) = reusable_polygon_account(root, cx);
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
            // Both accounts are new, and each setup was sent.
            let source = ExecutorOperationId::random().unwrap();
            let destination = ExecutorOperationId::random().unwrap();
            let receiver = Address::repeat_byte(0x61);
            let mut approval = SwapApproval {
                delivery: SwapDelivery::Bridge(BridgeDelivery {
                    provider: BridgeProvider::Across,
                    destination_chain: 137,
                    receiver,
                    destination_token: STUB_POLYGON_USDT,
                    surplus: BridgeSurplus::Reshield,
                    private: Some(BridgePrivateDelivery {
                        on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
                    }),
                }),
                tokens: Some(SwapApprovalTokens {
                    sell: Address::repeat_byte(1),
                    buy: Address::repeat_byte(2),
                }),
                accounts: Some(SwapApprovedAccounts {
                    source: SwapApprovedAccount {
                        address: None,
                        setup: true,
                    },
                    destination: Some(SwapApprovedAccount {
                        address: None,
                        setup: true,
                    }),
                }),
                ..test_approval()
            };
            // Each setup's fee limit as the swap's review approved it. The destination's is in
            // USDC, with an amount far above the retry's fee in base units of another token.
            let source_limit = SwapSetupFeeLimit {
                token: STUB_USDC,
                amount: U256::from(7),
            };
            approval.bounds.source_setup_fee = Some(source_limit);
            approval.bounds.destination_setup_fee = Some(SwapSetupFeeLimit {
                token: STUB_POLYGON_USDC,
                amount: U256::from(u64::MAX),
            });
            executors
                .claim_swap_pair(SwapPairClaim {
                    id: SwapUseId::first(source),
                    source: SwapAccountChoice::New(source),
                    delegate: origin_delegate,
                    purpose_summary: Some("Private swap".into()),
                    assets: Vec::new(),
                    approval,
                    destination: Some(SwapDestinationClaim {
                        chain_id: 137,
                        account: SwapAccountChoice::New(destination),
                        delegate: polygon_delegate,
                        destination_token: STUB_POLYGON_USDT,
                    }),
                })
                .unwrap();
            executors
                .bind_address(source, Address::repeat_byte(0x33))
                .unwrap();
            polygon_store.bind_address(destination, receiver).unwrap();
            record_setup(
                executors,
                source,
                origin_delegate,
                B256::repeat_byte(0x43),
                Vec::new(),
            );
            record_setup(
                &polygon_store,
                destination,
                polygon_delegate,
                B256::repeat_byte(0x44),
                Vec::new(),
            );
            polygon_owner
                .plan_swaps_from_note_for_tests(STUB_POLYGON_USDT, U256::from(100_000_000));
            let offer = setup_offer(root, 137, STUB_POLYGON_USDT, U256::from(10), cx);
            let estimate = runtime
                .block_on(Box::pin(polygon_owner.estimate_swap_setup_fee(
                    &polygon,
                    None,
                    offer,
                    &operation_client(root, runtime, cx),
                )))
                .unwrap();
            let waku = idle_waku(root, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.reload_destinations(cx);
                    // No broadcaster answered either setup, and each account was read since.
                    for side in [SetupSide::Origin, SetupSide::Destination] {
                        drop(swaps.start_setup_progress(source, side, setup_broadcaster(), cx));
                        swaps.finish_setup_progress(
                            source,
                            side,
                            Some(Ok(&PublicBroadcasterResultKind::TimedOut)),
                        );
                    }
                    let tracking = swaps.tracking.entry(source).or_default();
                    tracking.setup = Some(wallet_ops::SwapSetupStatus::Pending);
                    tracking.destination_setup = Some(wallet_ops::SwapSetupStatus::Pending);

                    let retry = swaps
                        .setup_retry_approval(source, SetupSide::Destination, &estimate, waku, cx)
                        .unwrap();
                    assert_eq!((retry.chain_id, retry.account), (137, destination));
                    // The estimate is in USDT, which amounts in USDC don't compare with.
                    let saved = &retry.approval.as_ref().unwrap().bounds;
                    assert_eq!(
                        saved.destination_setup_fee,
                        Some(SwapSetupFeeLimit {
                            token: STUB_POLYGON_USDT,
                            amount: retry.maximum_private_fee,
                        })
                    );
                    assert_eq!(saved.source_setup_fee, Some(source_limit));
                    let authorization = DesktopPrivateSpendAuthorization::VaultPassword(
                        zeroize::Zeroizing::new("approved".to_owned()),
                    );
                    swaps.submit_setup_retry(retry, authorization, window, cx);

                    let tracking = &swaps.tracking[&source];
                    let ended = |side| tracking.setup_progress(side).and_then(SetupProgress::end);
                    assert_eq!(
                        ended(SetupSide::Origin),
                        Some(SetupProgressEnd::TimedOut),
                        "the swap's own setup keeps its stages"
                    );
                    assert_eq!(tracking.setup, Some(wallet_ops::SwapSetupStatus::Pending));
                    assert!(
                        tracking.setup_progress(SetupSide::Destination).is_some()
                            && ended(SetupSide::Destination).is_none(),
                        "the destination setup's stages start again"
                    );
                    assert!(tracking.destination_setup.is_none());
                    if let Some(job) = swaps.job.take() {
                        assert_eq!(job.kind, SwapJobKind::Setup);
                        job.abort.abort();
                    }
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// A private Bridge preparation that stopped before its accounts were derived leaves the swap's
/// approval with the placeholder receiver and no account address. The store saves an approval
/// only for an own account that has an address, so the destination setup's retry waits for
/// the swap's own. Each retry then binds what is derived by then: the swap's own saves a raised
/// fee limit with the swap's own account, and the destination's saves the receiver, after which
/// the pair resolves. An unbound receiver hides neither a setup already included on the
/// destination account nor that account's retirement.
#[gpui::test]
fn an_interrupted_bridge_preparation_is_bound_by_its_setup_retries(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        ExecutorNonceObservation, SwapApprovalTokens, SwapDestinationClaim, SwapPairClaim,
    };
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, executors, _, runtime, store, cx| {
            cx.update(|_, cx| {
                root.update(cx, |root, _| enable_stub_chain(root, &stubs, 137));
            });
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let (polygon_store, _) = reusable_polygon_account(root, cx);
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
            // The approval a preparation saves with its claim, before either account is derived.
            let mut unbound = SwapApproval {
                delivery: SwapDelivery::Bridge(BridgeDelivery {
                    provider: BridgeProvider::Across,
                    destination_chain: 137,
                    receiver: Address::ZERO,
                    destination_token: STUB_POLYGON_USDT,
                    surplus: BridgeSurplus::Reshield,
                    private: Some(BridgePrivateDelivery {
                        on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
                    }),
                }),
                tokens: Some(SwapApprovalTokens {
                    sell: Address::repeat_byte(1),
                    buy: Address::repeat_byte(2),
                }),
                accounts: Some(SwapApprovedAccounts {
                    source: SwapApprovedAccount {
                        address: None,
                        setup: true,
                    },
                    destination: Some(SwapApprovedAccount {
                        address: None,
                        setup: true,
                    }),
                }),
                ..test_approval()
            };
            unbound.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                token: STUB_USDC,
                amount: U256::ONE,
            });
            unbound.bounds.destination_setup_fee = Some(SwapSetupFeeLimit {
                token: STUB_POLYGON_USDT,
                amount: U256::ONE,
            });
            let claim = |destination_delegate| {
                let source = ExecutorOperationId::random().unwrap();
                let destination = ExecutorOperationId::random().unwrap();
                executors
                    .claim_swap_pair(SwapPairClaim {
                        id: SwapUseId::first(source),
                        source: SwapAccountChoice::New(source),
                        delegate: origin_delegate,
                        purpose_summary: Some("Private swap".into()),
                        assets: Vec::new(),
                        approval: unbound.clone(),
                        destination: Some(SwapDestinationClaim {
                            chain_id: 137,
                            account: SwapAccountChoice::New(destination),
                            delegate: destination_delegate,
                            destination_token: STUB_POLYGON_USDT,
                        }),
                    })
                    .unwrap();
                (source, destination)
            };
            let retry = |cx: &mut gpui::VisualTestContext, source, side| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    swaps.reload_destinations(cx);
                    swaps.setup_retry_availability(swaps.record(source).unwrap(), side, cx)
                })
            };
            let owner = swaps.read_with(cx, |swaps, _| Arc::clone(swaps.private_owner().unwrap()));

            // Neither account has an address yet.
            let (source, destination) = claim(polygon_delegate);
            let swap_use = SwapUseId::first(source);
            assert_eq!(retry(cx, source, SetupSide::Origin), Some(Ok(())));
            assert_eq!(
                retry(cx, source, SetupSide::Destination),
                Some(Err("Set up the account on Ethereum first.".into()))
            );
            assert!(
                owner
                    .bind_swap_use_accounts(source, swap_use, None)
                    .is_err()
            );

            // The retry of the swap's own setup derives that account, and saves the fee limit
            // its review raised. The destination account isn't derived, so the receiver stays.
            let own = Address::repeat_byte(0x33);
            executors.bind_address(source, own).unwrap();
            let mut raised = unbound.clone();
            raised.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                token: STUB_USDC,
                amount: U256::from(2),
            });
            let saved = owner
                .bind_swap_use_accounts(source, swap_use, Some(raised.clone()))
                .unwrap();
            let mut accounts = unbound.accounts.unwrap();
            accounts.source.address = Some(own);
            raised.accounts = Some(accounts);
            assert_eq!(saved, raised, "no other term changes");
            assert_eq!(retry(cx, source, SetupSide::Destination), Some(Ok(())));

            // The destination setup's retry derives the destination account, which the saved
            // approval then names as the delivery's receiver.
            let receiver = Address::repeat_byte(0x61);
            polygon_store.bind_address(destination, receiver).unwrap();
            let bound = owner
                .bind_swap_use_accounts(source, swap_use, None)
                .unwrap();
            let SwapDelivery::Bridge(delivery) = bound.delivery else {
                panic!("the approval keeps its Bridge delivery");
            };
            assert_eq!(delivery.receiver, receiver);
            assert_eq!(bound.bounds, raised.bounds);
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.reload_destinations(cx);
                let record = swaps.record(source).unwrap();
                assert_eq!(record.swap_approval(), Some(&bound));
                let accounts = swaps.bound_accounts(record, swap_use);
                assert!(accounts.is_some() && accounts == bound.accounts);
                let swap = model::SwapIdentity {
                    operation: source,
                    swap_use,
                };
                assert_eq!(
                    swaps
                        .swap_destination_account(swap, delivery)
                        .map(ExecutorRecord::operation),
                    Some(destination)
                );
            });

            // Another interrupted swap, whose destination account was derived and its setup
            // sent there. That setup ran for a delegate this wallet doesn't accept on Polygon,
            // so its consumed nonce doesn't make the account ready.
            let unaccepted = Address::repeat_byte(0x99);
            let (source, destination) = claim(unaccepted);
            executors
                .bind_address(source, Address::repeat_byte(0x34))
                .unwrap();
            polygon_store
                .bind_address(destination, Address::repeat_byte(0x62))
                .unwrap();
            record_setup(
                &polygon_store,
                destination,
                unaccepted,
                B256::repeat_byte(0x44),
                Vec::new(),
            );
            confirm_setup(
                &polygon_store,
                destination,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(12, B256::repeat_byte(12)),
                    U256::ONE,
                ),
            );
            assert_eq!(
                retry(cx, source, SetupSide::Destination),
                Some(Err(SETUP_ALREADY_INCLUDED.into()))
            );
            polygon_store.retire(destination).unwrap();
            assert_eq!(retry(cx, source, SetupSide::Destination), None);
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}
