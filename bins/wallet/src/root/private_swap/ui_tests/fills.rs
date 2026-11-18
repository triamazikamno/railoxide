use super::*;

/// An open Across order of 99 for [`BRIDGE_MINIMUM`] on Polygon to the address-book entry
/// "Cold wallet", accepted by the orderbook and valid for ten more minutes, with the synced
/// head at block 400. Returns its UID and the account's observation.
fn open_across_order(
    root: &Entity<WalletRoot>,
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    cx: &mut gpui::VisualTestContext,
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    use wallet_ops::vault::{BridgeDelivery, BridgeProvider, BridgeSurplus, SwapSubmissionStatus};

    let receiver = Address::repeat_byte(0x51);
    let (uid, observed) = placed_order(
        executors,
        operation,
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver,
            destination_token: STUB_POLYGON_USDC,
            surplus: BridgeSurplus::Reshield,
            private: None,
        }),
        PlacedOrder {
            uid_hash: 0x11,
            hook_hash: 7,
            valid_to: u32::try_from(now_unix()).unwrap() + 600,
            speeds_up: None,
        },
        |_| {},
    );
    executors
        .record_swap_submission(operation, uid, SwapSubmissionStatus::Accepted)
        .unwrap();
    cx.update(|_, cx| {
        root.update(cx, |root, _| {
            root.public_address_book = vec![cold_wallet_entry(receiver)];
            let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1) else {
                panic!("ready fixture")
            };
            sync_tip.head_block = Some(400);
        });
    });
    (uid, observed)
}

/// The orderbook's report that `uid` filled in `block`, with what the same pass learned early
/// about its settlement and delivery.
fn reported_fill(
    operation: ExecutorOperationId,
    uid: broadcaster_core::contracts::cow::OrderUid,
    block: u64,
    early: Option<&SwapEarlyHint>,
) -> HintResult {
    HintResult {
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
        early: early.copied(),
    }
}

/// What a pass holds about an Across order once it read the trade and the deposit in `block`
/// and its provider answered `report`.
fn early_hint(block: u64, report: Option<wallet_ops::SwapBridgeReport>) -> SwapEarlyHint {
    let block_hash = alloy::eips::BlockNumHash::new(block, B256::repeat_byte(98));
    SwapEarlyHint {
        trade_block: Some(block),
        block_read: true,
        settlement: Some(wallet_ops::SwapSettlementHint {
            block: block_hash,
            bought: U256::from(99),
            shielded: None,
            shield_transaction: None,
            handoff: Some(wallet_ops::SwapHandoffHint {
                block: block_hash,
                deposit_id: Some(U256::from(7)),
                sent: U256::from(99),
            }),
        }),
        report,
        reads_fill: false,
    }
}

/// A swap back to the private balance whose fill the orderbook reports has its reported block
/// read once, before the settlement is final, and shows the trade and the shield that block
/// holds.
#[gpui::test]
fn a_reported_private_fill_reads_its_block_once(cx: &mut TestAppContext) {
    use wallet_ops::vault::SwapSubmissionStatus;

    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let (uid, _) = placed_order(
            executors,
            operation,
            SwapDelivery::Reshield,
            PlacedOrder {
                uid_hash: 0x11,
                hook_hash: 7,
                valid_to: u32::try_from(now_unix()).unwrap() + 600,
                speeds_up: None,
            },
            |_| {},
        );
        executors
            .record_swap_submission(operation, uid, SwapSubmissionStatus::Accepted)
            .unwrap();
        let asked = |swaps: &PrivateSwapsView, cx: &App| {
            swaps
                .next_order_hints(cx)
                .map_or_else(Vec::new, |(_, _, requests)| {
                    requests
                        .iter()
                        .map(|request| request.early)
                        .collect::<Vec<_>>()
                })
        };
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                assert_eq!(asked(swaps, cx), [None]);

                swaps.apply_order_hints(vec![reported_fill(operation, uid, 398, None)], cx);
                let unread = SwapEarlyHint {
                    trade_block: Some(398),
                    block_read: false,
                    settlement: None,
                    report: None,
                    reads_fill: false,
                };
                assert_eq!(asked(swaps, cx), [Some((None, unread))]);

                let read = SwapEarlyHint {
                    block_read: true,
                    settlement: Some(wallet_ops::SwapSettlementHint {
                        block: alloy::eips::BlockNumHash::new(398, B256::repeat_byte(98)),
                        bought: U256::from(99),
                        shielded: Some(U256::from(98)),
                        shield_transaction: None,
                        handoff: None,
                    }),
                    ..unread
                };
                swaps.apply_order_hints(vec![reported_fill(operation, uid, 398, Some(&read))], cx);
                // The read block needs no further request, whatever a request backs off to.
                swaps
                    .tracking
                    .get_mut(&operation)
                    .unwrap()
                    .bridge_polls
                    .clear();
                assert_eq!(asked(swaps, cx), [None]);

                let record = swaps.record(operation).unwrap();
                let (_, buy) = swap_tokens(record).unwrap();
                let hint = swaps.labels(record, cx).fill_hint.unwrap();
                assert!(hint.detected);
                assert_eq!(
                    hint.bought,
                    Some(swaps.token_amount(buy, U256::from(99), cx))
                );
                assert_eq!(
                    hint.shield,
                    model::SwapSeen::Detected(Some(swaps.token_amount(buy, U256::from(98), cx)))
                );
            });
        });
    });
}

/// A Bridge order whose fill the orderbook reports is asked about before its settlement is
/// final: its trade and deposit show as read from the reported block and its delivery as
/// reported, while the trade still counts confirmations. What was
/// learned belongs to the reported block, so another block drops it and is read again. Once the
/// hand-off is recorded, the order is tracked as handed off, once a pass, and nothing learned
/// early is kept.
#[gpui::test]
fn a_reported_bridge_fill_is_asked_about_early_until_its_hand_off_is_recorded(
    cx: &mut TestAppContext,
) {
    use crate::root::public_action::PublicActionStepStatus::{Done, NotStarted, Pending};
    use wallet_ops::SwapBridgeReport;
    use wallet_ops::vault::BridgeProvider;

    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let (uid, observed) = open_across_order(root, executors, operation, cx);
        // What each request of the next pass asks early, and the handed-off orders it polls.
        let asked = |swaps: &PrivateSwapsView, cx: &App| {
            swaps
                .next_order_hints(cx)
                .map_or_else(Vec::new, |(_, _, requests)| {
                    requests
                        .iter()
                        .map(|request| {
                            let polled = request.bridges.iter().map(|(uid, _)| *uid);
                            (request.early, polled.collect::<Vec<_>>())
                        })
                        .collect::<Vec<_>>()
                })
        };
        let unread = |block| {
            let hint = SwapEarlyHint {
                trade_block: Some(block),
                block_read: false,
                settlement: None,
                report: None,
                reads_fill: false,
            };
            Some((Some(BridgeProvider::Across), hint))
        };
        // The Traded step's detail and the destination step, as the swap's labels give them.
        let shown = |swaps: &PrivateSwapsView, cx: &App| {
            let record = swaps.record(operation).unwrap();
            let stage = swaps.stage(record);
            assert_eq!(stage, SwapStage::Order(SwapOrderState::Open));
            let steps = model::swap_steps(stage, &swaps.labels(record, cx));
            let traded = steps.iter().find(|step| step.label == "Traded").unwrap();
            let last = steps.last().unwrap();
            (
                traded.detail.clone(),
                last.label.clone(),
                last.status,
                last.detail.clone(),
            )
        };
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let depth = 400 - swaps.confirmed_block(cx).unwrap();
                let confirming = |count: u64| format!("Confirming ({count}/{depth})");
                let expected = format!(
                    "{} to Cold wallet",
                    swaps.network_token_amount(137, STUB_POLYGON_USDC, BRIDGE_MINIMUM, cx)
                );
                assert_eq!(asked(swaps, cx), [(None, Vec::new())]);

                // The fill is reported two blocks deep, and its block is read at once.
                swaps.apply_order_hints(vec![reported_fill(operation, uid, 398, None)], cx);
                assert_eq!(asked(swaps, cx), [(unread(398), Vec::new())]);

                // The relayer filled: the delivery shows as Across's word, and the trade
                // still counts. A reported end is asked about no more.
                let delivered = Some(SwapBridgeReport::Delivered {
                    amount_out: Some(BRIDGE_MINIMUM),
                    seen: None,
                    transaction: None,
                });
                swaps.apply_order_hints(
                    vec![reported_fill(
                        operation,
                        uid,
                        398,
                        Some(&early_hint(398, delivered)),
                    )],
                    cx,
                );
                let record = swaps.record(operation).unwrap();
                let (_, buy) = swap_tokens(record).unwrap();
                let sent = swaps.token_amount(buy, U256::from(99), cx);
                assert_eq!(
                    shown(swaps, cx),
                    (
                        format!("{} · {sent} bought", confirming(2)),
                        "Delivered on Polygon · reported by Across".to_owned(),
                        Done,
                        expected.clone()
                    )
                );
                assert_eq!(
                    swaps.labels(record, cx).bridge.unwrap().deposit,
                    model::SwapSeen::Detected(Some(sent))
                );
                assert_eq!(asked(swaps, cx), [(None, Vec::new())]);

                // The trade is reported in another block while an answer about the first
                // one arrives. The delivery waits again, and the new block is read.
                swaps.apply_order_hints(
                    vec![reported_fill(
                        operation,
                        uid,
                        397,
                        Some(&early_hint(398, delivered)),
                    )],
                    cx,
                );
                assert_eq!(
                    shown(swaps, cx),
                    (
                        confirming(3),
                        "Delivered on Polygon".to_owned(),
                        NotStarted,
                        expected
                    )
                );
                assert_eq!(asked(swaps, cx), [(unread(397), Vec::new())]);

                // Across doesn't know the deposit yet, so the next request backs off.
                swaps.apply_order_hints(
                    vec![reported_fill(
                        operation,
                        uid,
                        397,
                        Some(&early_hint(397, Some(SwapBridgeReport::Pending))),
                    )],
                    cx,
                );
                let (_, label, status, _) = shown(swaps, cx);
                assert_eq!((label.as_str(), status), ("Delivered on Polygon", Pending));
                assert_eq!(asked(swaps, cx), [(None, Vec::new())]);
            });
        });

        // Verification records the hand-off. The order is polled as handed off in the next
        // pass, whatever the early requests backed off to, and never also asked early.
        executors
            .record_swap_observations(
                operation,
                uid,
                bridge_observations(observed, Some(U256::from(7)), None),
            )
            .unwrap();
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let mut polygon = built_in_chain(137);
                polygon.enabled = true;
                put_chain(root, polygon);
            });
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert_eq!(
                    swaps.stage(record),
                    SwapStage::Order(SwapOrderState::Bridging)
                );
                assert_eq!(swaps.labels(record, cx).bridge.unwrap().report, None);
                assert_eq!(asked(swaps, cx), [(None, vec![uid])]);

                // Both orders of the next result start here: the hand-off is recorded, and the
                // early answers and their back-off are still held.
                let held_hint = swaps.tracking[&operation].early_hints[&uid];
                let held_poll = swaps.tracking[&operation].bridge_polls[&uid];
                assert!(held_poll.attempts > 0);

                // An early answer about the reported block arrives late. It is dropped with its
                // back-off, and the order is polled as handed off at once.
                swaps.apply_order_hints(
                    vec![reported_fill(
                        operation,
                        uid,
                        397,
                        Some(&early_hint(397, Some(SwapBridgeReport::Pending))),
                    )],
                    cx,
                );
                assert!(swaps.tracking[&operation].early_hints.is_empty());
                assert_eq!(asked(swaps, cx), [(None, vec![uid])]);

                // The routine poll comes first instead and has no outcome. It doesn't inherit
                // the early back-off, so the order is polled again at once.
                let tracking = swaps.tracking.get_mut(&operation).unwrap();
                tracking.early_hints.insert(uid, held_hint);
                tracking.bridge_polls.insert(uid, held_poll);
                let polled = || HintResult {
                    bridges: vec![(uid, false)],
                    report: None,
                    ..reported_fill(operation, uid, 397, None)
                };
                swaps.apply_order_hints(vec![polled()], cx);
                assert!(swaps.tracking[&operation].early_hints.is_empty());
                assert_eq!(asked(swaps, cx), [(None, vec![uid])]);

                // From then on its polls back off from the base, and the cleanup keeps them.
                swaps.apply_order_hints(vec![polled()], cx);
                assert_eq!(swaps.tracking[&operation].bridge_polls[&uid].attempts, 1);
                assert!(asked(swaps, cx).iter().all(|(_, polled)| polled.is_empty()));
                swaps.apply_order_hints(Vec::new(), cx);
                assert_eq!(swaps.tracking[&operation].bridge_polls[&uid].attempts, 1);
            });
        });
    });
}

/// A private Across order's reported delivery is asked about again, with its destination chain,
/// until the fill block there was read. The block's shield then ends the early requests and
/// shows as detected with the private credit.
#[gpui::test]
fn a_reported_private_bridge_delivery_is_asked_about_until_its_fill_block_is_read(
    cx: &mut TestAppContext,
) {
    use crate::root::public_action::PublicActionStepStatus::Done;
    use wallet_ops::vault::SwapSubmissionStatus;
    use wallet_ops::{SwapBridgeReport, SwapDeliverySeen};

    with_swap_view(cx, |root, swaps, origin, _, _, cx| {
        // Polygon is enabled, and Ethereum's synced head is at block 400.
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                let mut polygon = built_in_chain(137);
                polygon.enabled = true;
                put_chain(root, polygon);
                let Some(ChainUtxoState::Ready { sync_tip, .. }) = root.chain_states.get_mut(&1)
                else {
                    panic!("ready fixture")
                };
                sync_tip.head_block = Some(400);
            });
        });
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
        let destination = ExecutorStore::new(db, view, 137).unwrap();
        let PlacedPrivateBridge { operation, uid, .. } = placed_private_bridge(
            origin,
            &destination,
            origin_delegate,
            delegate,
            STUB_POLYGON_USDC,
        );
        origin
            .record_swap_submission(operation, uid, SwapSubmissionStatus::Accepted)
            .unwrap();
        // The early request of the next pass, and whether it carries the destination chain.
        let asked = |swaps: &PrivateSwapsView, cx: &App| {
            swaps
                .next_order_hints(cx)
                .map_or_else(Vec::new, |(_, _, requests)| {
                    requests
                        .iter()
                        .map(|request| {
                            let (_, hint) = request.early?;
                            Some((hint, request.destination.is_some()))
                        })
                        .collect::<Vec<_>>()
                })
        };
        // What a pass holds once Across reports the delivery and the fill block showed `seen`.
        let delivered = |seen| SwapEarlyHint {
            reads_fill: true,
            ..early_hint(
                398,
                Some(SwapBridgeReport::Delivered {
                    amount_out: Some(BRIDGE_MINIMUM),
                    seen,
                    transaction: None,
                }),
            )
        };
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                // Across reports the delivery before its fill block could be read. The request
                // backs off and then comes again.
                let unread = delivered(None);
                swaps
                    .apply_order_hints(vec![reported_fill(operation, uid, 398, Some(&unread))], cx);
                assert_eq!(asked(swaps, cx), [None]);
                swaps
                    .tracking
                    .get_mut(&operation)
                    .unwrap()
                    .bridge_polls
                    .clear();
                assert_eq!(asked(swaps, cx), [Some((unread, true))]);

                // The fill block shows the shield, and nothing is asked early any more.
                let shielded = delivered(Some(SwapDeliverySeen::Shielded {
                    amount: BRIDGE_MINIMUM,
                }));
                swaps.apply_order_hints(
                    vec![reported_fill(operation, uid, 398, Some(&shielded))],
                    cx,
                );
                assert!(swaps.tracking[&operation].bridge_polls.is_empty());
                assert_eq!(asked(swaps, cx), [None]);

                let record = swaps.record(operation).unwrap();
                let steps = model::swap_steps(swaps.stage(record), &swaps.labels(record, cx));
                let last = steps.last().unwrap();
                // The fixture's approval binds no shield fee, so all of it is credited.
                assert_eq!(
                    (last.label.as_str(), last.status, last.detail.clone()),
                    (
                        "Private on Polygon",
                        Done,
                        format!(
                            "Shield detected · {}",
                            swaps.network_token_amount(137, STUB_POLYGON_USDC, BRIDGE_MINIMUM, cx)
                        )
                    )
                );
            });
        });
    });
}

/// A provider's report about a reported fill changes the destination step, the card and the
/// note, and nothing else: the detail keeps the footer of the reported fill alone, without an
/// outcome and without retry or recovery.
#[gpui::test]
fn a_bridge_providers_early_report_changes_the_copy_and_offers_no_action(cx: &mut TestAppContext) {
    use crate::root::public_action::PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
    use wallet_ops::SwapBridgeReport;

    const FOOTER: [&str; 6] = [
        "swap-progress-check",
        "swap-progress-continue",
        "swap-progress-retry",
        "swap-progress-recover",
        "swap-progress-recover-destination",
        "swap-progress-remove",
    ];
    const NOTHING_TO_DO: &str = "Nothing more to do. The result is confirmed after";
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        let (uid, _) = open_across_order(root, executors, operation, cx);
        // The footer of the reported fill before its provider answered.
        let mut footer = None;
        for (report, last, card, note) in [
            (
                None,
                ("Delivered on Polygon", NotStarted),
                "Traded · confirming",
                NOTHING_TO_DO,
            ),
            (
                Some(SwapBridgeReport::Pending),
                ("Delivered on Polygon", Pending),
                "Traded · confirming",
                NOTHING_TO_DO,
            ),
            (
                Some(SwapBridgeReport::Delivered {
                    amount_out: None,
                    seen: None,
                    transaction: None,
                }),
                ("Delivered on Polygon · reported by Across", Done),
                "Delivered to Cold wallet on Polygon · confirming",
                NOTHING_TO_DO,
            ),
            (
                Some(SwapBridgeReport::Refunded),
                ("Refund · reported by Across", Warning),
                "Traded · confirming",
                "The wallet acts on this once the result is confirmed, after",
            ),
        ] {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.reload_records();
                    let early = report.map(|report| early_hint(398, Some(report)));
                    swaps.apply_order_hints(
                        vec![reported_fill(operation, uid, 398, early.as_ref())],
                        cx,
                    );
                    let record = swaps.record(operation).unwrap();
                    let stage = swaps.stage(record);
                    assert_eq!(stage, SwapStage::Order(SwapOrderState::Open));
                    let labels = swaps.labels(record, cx);
                    let steps = model::swap_steps(stage, &labels);
                    let shown = steps.last().unwrap();
                    assert_eq!((shown.label.as_str(), shown.status), last, "{report:?}");
                    let line = model::swap_card_line(stage, &labels);
                    assert_eq!(line.detail, card, "{report:?}");
                    assert!(!line.attention, "{report:?}");
                    let shown = model::reported_fill_note(stage, &labels).unwrap();
                    assert!(shown.starts_with(note), "{shown}");
                    swaps.show_detail(operation, window, cx);
                });
                window.draw(cx).clear(cx);
            });
            // The outcome and every action wait for what observation records.
            assert!(cx.debug_bounds("swap-outcome").is_none(), "{report:?}");
            let offered = FOOTER.map(|action| cx.debug_bounds(action).is_some());
            assert_eq!(
                offered[2..5],
                [false; 3],
                "a report offers no retry or recovery: {report:?}"
            );
            assert_eq!(*footer.get_or_insert(offered), offered, "{report:?}");
        }
    });
}

/// An Across Bridge swap names its destination and provider from the hand-off through each
/// outcome, and its detail shows the bridge's facts rather than a private-balance outcome.
/// A trade without its deposit leaves the hand-off unverified until an account read finds the
/// bought token in the stealth account, which alone offers recovery. Recover… for a refund
/// waits until a check verified the refund and finds funds in the stealth account, and
/// recovery opens with that check's balance.
#[gpui::test]
fn across_bridge_swaps_show_the_hand_off_and_each_outcome(cx: &mut TestAppContext) {
    use crate::root::public_action::PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::{
        BridgeDelivery, BridgeProvider, BridgeSurplus, SwapBridgeOutcome, SwapOrderObservations,
    };

    let receiver = Address::repeat_byte(0x51);
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
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
        // The settlement showed no deposit, and no account read has found the payout yet.
        let unverified = SwapOrderObservations {
            undelivered: None,
            ..not_sent.clone()
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
                    destination_transaction: None,
                })),
                SwapOrderState::Done,
                ("Delivered on Polygon · reported by Across", Done),
                "Delivered · reported by Across",
            ),
            (
                handed_off(Some(verified.clone())),
                SwapOrderState::Done,
                ("Delivered on Polygon · verified", Done),
                "Delivered",
            ),
            (
                unverified,
                SwapOrderState::Traded,
                ("Delivered on Polygon", NotStarted),
                "Traded",
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
                    // A trade without its deposit reads as unverified. Only the account read
                    // names the stealth account as holding the token and offers recovery.
                    let step_shown = |label: &str, status| {
                        steps
                            .iter()
                            .any(|step| step.label == label && step.status == status)
                    };
                    assert_eq!(
                        (
                            step_shown("Bridge deposit unverified", Pending),
                            step_shown("Not sent to the bridge", Warning)
                        ),
                        (
                            state == SwapOrderState::Traded,
                            state == SwapOrderState::NotDelivered
                        ),
                        "{state:?}"
                    );
                    if state == SwapOrderState::Traded {
                        assert!(!model::swap_actions(stage, false).recover);
                    }
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
            // A swap with one order has no Orders section.
            assert!(cx.debug_bounds("swap-attempts").is_none(), "{state:?}");
            if matches!(state, SwapOrderState::Traded | SwapOrderState::NotDelivered) {
                assert!(
                    cx.debug_bounds("swap-progress-check").is_some(),
                    "{state:?}"
                );
                assert_eq!(
                    cx.debug_bounds("swap-progress-recover").is_some(),
                    state == SwapOrderState::NotDelivered,
                    "{state:?}"
                );
            }
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
    across_refund_recovery(cx, false);
}

/// The same when a retry holds the trade and the expired first attempt, whose signed terms the
/// deposit matched, holds the hand-off and its refund. The swap's stage is the retry's.
#[gpui::test]
fn a_refund_on_another_attempt_is_recovered_only_after_its_verified_refund(
    cx: &mut TestAppContext,
) {
    across_refund_recovery(cx, true);
}

fn across_refund_recovery(cx: &mut TestAppContext, split: bool) {
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
        SwapObservation, SwapOrderObservations, SwapPreHookDeath, SwapPreHookDeathCause,
    };

    let stubs = split.then(SwapStubs::start);
    with_approved_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
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
        // `uid` is the order that holds the hand-off, its outcome and its refund. Split, it
        // expired untraded and a retry at the same nonce holds the trade.
        let expired = split.then_some(SwapPreHookDeath {
            cause: SwapPreHookDeathCause::Expired,
            observation: SwapObservation {
                block: observed.block(),
                transaction_hash: None,
            },
        });
        let handed_off = |outcome, bridge_refund| {
            let whole = SwapOrderObservations {
                bridge_refund,
                ..bridge_observations(observed, Some(U256::from(7)), outcome)
            };
            match expired {
                Some(expired) => deposit_of_a_later_trade(&whole, expired),
                None => whole,
            }
        };
        if let Some(expired) = expired {
            retried_and_traded(executors, operation, uid, observed, expired, |_| {});
        }
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
            .record_swap_observations(operation, uid, handed_off(refunding.clone(), None))
            .unwrap();
        assert_eq!(
            stage(cx, &shields),
            SwapStage::Order(SwapOrderState::Refunding)
        );
        // Split, Check status asks Across about the expired attempt, which holds the deposit.
        // The retry holds no hand-off, and a check of it would ask nothing.
        if let Some(stubs) = &stubs {
            let orderbook = stub_orderbook(stubs, runtime);
            cx.update(|window, cx| {
                root.update(cx, |root, _| enable_stub_chain(root, stubs, 137));
                swaps.update(cx, |swaps, cx| {
                    let tracking = swaps.tracking.entry(operation).or_default();
                    tracking.bridge_clients = Some(stub_bridge_clients(stubs, &orderbook));
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
                    .any(|path| path.starts_with("/across/deposit")),
                "{:?}",
                stubs.bridge_requests()
            );
            assert_eq!(
                stage(cx, &shields),
                SwapStage::Order(SwapOrderState::Refunding)
            );
        }
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
