use super::*;

/// A retry approved a higher destination minimum than the expired first attempt, traded, and
/// its settlement made the first attempt's deposit. The swap is one entry that follows that
/// deposit: its delivery is held to the first attempt's minimum, the retry's minimum is named
/// as not applying, both orders are listed with their own terms, and a refund is recovered
/// from the first attempt's verified refund.
#[gpui::test]
fn a_delivery_under_an_earlier_attempts_terms_keeps_each_attempts_minimum(cx: &mut TestAppContext) {
    use crate::root::public_action::PublicActionStepStatus::{Done, Warning};
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::{
        BridgeDelivery, BridgeProvider, BridgeSurplus, SwapBridgeOutcome, SwapObservation,
        SwapOrderObservations, SwapPreHookDeath, SwapPreHookDeathCause,
    };

    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let bridge = BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver: Address::repeat_byte(0x51),
            destination_token: STUB_POLYGON_USDC,
            surplus: BridgeSurplus::Reshield,
            private: None,
        };
        let (uid, observed) = placed_swap(executors, operation, SwapDelivery::Bridge(bridge));
        let higher = BRIDGE_MINIMUM + U256::from(10_000_000);
        let expired = SwapPreHookDeath {
            cause: SwapPreHookDeathCause::Expired,
            observation: SwapObservation {
                block: observed.block(),
                transaction_hash: None,
            },
        };
        let retry = retried_and_traded(executors, operation, uid, observed, expired, |bounds| {
            bounds.destination_minimum = Some(higher);
        });
        let handed_off = |outcome| {
            deposit_of_a_later_trade(
                &bridge_observations(observed, Some(U256::from(7)), outcome),
                expired,
            )
        };
        executors
            .record_swap_observations(
                operation,
                uid,
                handed_off(Some(SwapBridgeOutcome::DeliveredVerified {
                    block: BlockNumHash::new(71_904_233, B256::repeat_byte(60)),
                    transaction_hash: B256::repeat_byte(61),
                    output_amount: BRIDGE_MINIMUM,
                    shielded: false,
                })),
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                let stage = swaps.stage(record);
                assert_eq!(stage, SwapStage::Order(SwapOrderState::Done));
                let amount =
                    |amount| swaps.network_token_amount(137, STUB_POLYGON_USDC, amount, cx);
                let (approved, unmet) = (amount(BRIDGE_MINIMUM), amount(higher));
                // The delivered amount and the minimum it is held to are the deposit's. The
                // order step keeps the latest order's ID.
                let labels = swaps.labels(record, cx);
                assert_eq!(labels.received.as_ref(), Some(&approved));
                assert_eq!(
                    labels.bridge.as_ref().unwrap().minimum.as_ref(),
                    Some(&approved)
                );
                assert_eq!(labels.order, Some(retry));
                let steps = model::swap_steps(stage, &labels);
                assert_eq!(
                    steps[1..]
                        .iter()
                        .map(|step| (step.label.as_str(), step.status))
                        .collect::<Vec<_>>(),
                    [
                        ("Order open", Done),
                        ("Bridge deposit", Done),
                        ("Delivered on Polygon · verified", Done),
                    ]
                );
                let latest = record.swap().unwrap().orders().last().unwrap();
                let attempts = model::SwapAttempts::ending_at(record, latest);
                let note = swaps.earlier_minimum_note(&attempts, bridge, cx).unwrap();
                assert!(note.contains(&approved) && note.contains(&unmet), "{note}");
                // Nothing else names the retry's minimum, so nothing says it was met.
                let card = model::swap_card_line(stage, &labels);
                assert!(
                    steps
                        .iter()
                        .flat_map(|step| [&step.label, &step.detail])
                        .chain([&card.title, &card.detail])
                        .all(|text| !text.contains(&unmet))
                );
                // Each order keeps its own minimum and its own ID to copy.
                let rows = swaps.attempt_rows(record, &attempts, cx);
                assert_eq!(
                    rows.iter()
                        .map(|row| (row.label.as_str(), row.order))
                        .collect::<Vec<_>>(),
                    [
                        ("Order 1 · Bridge deposit", uid),
                        ("Order 2 · Traded", retry)
                    ]
                );
                assert!(
                    rows[0]
                        .detail
                        .starts_with(&format!("Minimum {approved} · "))
                );
                assert!(rows[1].detail.starts_with(&format!("Minimum {unmet} · ")));
                // My orders lists the swap once.
                assert_eq!(
                    swaps
                        .order_rows_for_test(cx)
                        .iter()
                        .filter(|(meta, _)| meta.ends_with(" on Polygon · Across"))
                        .count(),
                    1
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-attempts").is_some());
        assert!(cx.debug_bounds("swap-bridge-earlier-minimum").is_some());

        // A refund of that deposit is the swap's too. Recover… waits for the refund verified
        // on the attempt that holds the deposit.
        let refunding = SwapStage::Order(SwapOrderState::Refunding);
        executors
            .record_swap_observations(
                operation,
                uid,
                handed_off(Some(SwapBridgeOutcome::Refunding)),
            )
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                swaps.tracking.entry(operation).or_default().stealth_balance =
                    Some((U256::from(99), observed.block()));
                let record = swaps.record(operation).unwrap();
                let steps = model::swap_steps(refunding, &swaps.labels(record, cx));
                let shown = steps.last().unwrap();
                assert_eq!(
                    (shown.label.as_str(), shown.status),
                    ("Refunding on Ethereum", Warning)
                );
            });
        });
        press_swap_recover(swaps, operation, refunding, cx);
        assert!(cx.debug_bounds("stealth-recovery-form").is_none());
        executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    bridge_refund: Some(SwapObservation {
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

/// The original order trades while its speed-up can still fill, and the settlement makes the
/// speed-up's deposit. The swap is one entry that follows the original's trade and that
/// deposit: its delivery is held to the speed-up's minimum, and each order keeps its own row.
#[gpui::test]
fn a_trade_delivered_by_its_speed_ups_deposit_follows_that_deposit(cx: &mut TestAppContext) {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        BridgeDelivery, BridgeProvider, BridgeSurplus, SwapBridgeOutcome, SwapOrderObservations,
    };

    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver: Address::repeat_byte(0x51),
            destination_token: STUB_POLYGON_USDC,
            surplus: BridgeSurplus::Reshield,
            private: None,
        });
        let lower = BRIDGE_MINIMUM - U256::from(1_000_000);
        let (original, observed) = placed_swap(executors, operation, delivery.clone());
        let (speed_up, _) = placed_order(
            executors,
            operation,
            delivery,
            PlacedOrder {
                uid_hash: 0x12,
                hook_hash: 0x21,
                valid_to: 20,
                speeds_up: Some(original),
            },
            |bounds| {
                // A speed-up asks for less of the bought token than its open order.
                bounds.buy_amount -= U256::from(1);
                bounds.destination_minimum = Some(lower);
            },
        );
        let handed_off = bridge_observations(
            observed,
            Some(U256::from(7)),
            Some(SwapBridgeOutcome::DeliveredVerified {
                block: BlockNumHash::new(71_904_233, B256::repeat_byte(60)),
                transaction_hash: B256::repeat_byte(61),
                output_amount: lower,
                shielded: false,
            }),
        );
        executors
            .record_swap_observations(
                operation,
                original,
                SwapOrderObservations {
                    pre_hook_executed: handed_off.pre_hook_executed,
                    traded: handed_off.traded,
                    trade_amounts: handed_off.trade_amounts,
                    ..Default::default()
                },
            )
            .unwrap();
        executors
            .record_swap_observations(
                operation,
                speed_up,
                SwapOrderObservations {
                    pre_hook_executed: None,
                    traded: None,
                    trade_amounts: None,
                    ..handed_off
                },
            )
            .unwrap();
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                assert_eq!(
                    swaps.stage(record),
                    SwapStage::Order(SwapOrderState::Done),
                    "the unfilled speed-up doesn't hide the delivery it made"
                );
                assert_eq!(model::record_swaps(record).len(), 1);
                assert_eq!(swaps.order_rows_for_test(cx).len(), 1);
                let delivered = swaps.network_token_amount(137, STUB_POLYGON_USDC, lower, cx);
                let labels = swaps.labels(record, cx);
                assert_eq!(labels.order, Some(original));
                assert_eq!(labels.received.as_ref(), Some(&delivered));
                assert_eq!(
                    labels.bridge.as_ref().unwrap().minimum.as_ref(),
                    Some(&delivered)
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
                        ("Order 1 · Traded", original),
                        ("Order 2 · Bridge deposit", speed_up)
                    ]
                );
            });
        });
    });
}

/// Check status on an Across refund asks Across about the deposit again, on the swap's own
/// route, since the deposit may have been filled after all. The refund is on this network, so
/// a failed provider check still leaves the balance check to run.
#[gpui::test]
fn checking_an_across_refund_asks_across_before_the_balance(cx: &mut TestAppContext) {
    use wallet_ops::vault::{BridgeDelivery, BridgeProvider, BridgeSurplus, SwapBridgeOutcome};

    let stubs = SwapStubs::start();
    with_approved_swap_view(cx, |root, swaps, executors, operation, runtime, cx| {
        let delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 137,
            receiver: Address::repeat_byte(0x51),
            destination_token: STUB_POLYGON_USDC,
            surplus: BridgeSurplus::Reshield,
            private: None,
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
                let error = swaps.tracking[&operation].check_error.clone().unwrap();
                assert!(!error.contains("Across"), "{error}");
            });
        });
    });
}
