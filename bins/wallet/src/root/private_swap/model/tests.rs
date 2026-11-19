use super::*;

fn labels() -> SwapLabels {
    SwapLabels {
        pair: "1.00 WETH for USDC".into(),
        sell: "1.00 WETH".into(),
        buy_symbol: "USDC".into(),
        expires: Some("14:32".into()),
        lapsed: false,
        fill_hint: None,
        received: None,
        receiver: None,
        minimum: None,
        uncovered_gas: None,
        order: None,
        shield_transaction: None,
        settlement_transaction: None,
        bridge: None,
    }
}

const ENDED: [SwapPreHookDeathCause; 5] = [
    SwapPreHookDeathCause::Expired,
    SwapPreHookDeathCause::Cancellation,
    SwapPreHookDeathCause::Recovery,
    SwapPreHookDeathCause::OlderPostHook,
    SwapPreHookDeathCause::Unknown,
];

fn every_stage() -> Vec<SwapStage> {
    let mut stages = vec![
        SwapStage::SetupNotSent,
        SwapStage::SetupRetired,
        SwapStage::SetupSubmitting,
        SwapStage::SetupPending,
        SwapStage::SetupFailed,
        SwapStage::Ready,
        SwapStage::Approved,
        SwapStage::SubmissionPending,
        SwapStage::SubmissionRejected,
        SwapStage::Recovered,
        SwapStage::Order(SwapOrderState::Open),
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }),
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: true }),
        SwapStage::Order(SwapOrderState::Traded),
        SwapStage::Order(SwapOrderState::Bridging),
        SwapStage::Order(SwapOrderState::Done),
        SwapStage::Order(SwapOrderState::NotDelivered),
        SwapStage::Order(SwapOrderState::Refunding),
        SwapStage::Order(SwapOrderState::NeedsAttention),
        SwapStage::Order(SwapOrderState::HeldOnDestination),
    ];
    stages.extend(
        ENDED
            .into_iter()
            .map(|cause| SwapStage::Order(SwapOrderState::AttemptEnded(cause))),
    );
    stages
}

#[test]
fn only_the_order_step_carries_the_order_id() {
    let uid = OrderUid([7; 56].into());
    let private = SwapLabels {
        order: Some(uid),
        ..labels()
    };
    let external = SwapLabels {
        receiver: Some("Treasury".into()),
        ..private.clone()
    };
    let bridged = SwapLabels {
        bridge: Some(SwapBridgeLabels {
            provider: BridgeProvider::Across,
            network: "Polygon".into(),
            token: "USDC".into(),
            non_evm: false,
            origin: "Arbitrum One".into(),
            receiver: "Treasury".into(),
            sent: None,
            minimum: None,
            private: None,
            reported: false,
            delivered_transaction: None,
            destination_transaction: None,
            handoff_transaction: None,
            refund_transaction: None,
            deposit: SwapSeen::Looking,
            report: None,
        }),
        ..private.clone()
    };
    for stage in every_stage() {
        let has_order = matches!(
            stage,
            SwapStage::SubmissionPending | SwapStage::SubmissionRejected | SwapStage::Order(_)
        );
        for labels in [&private, &external, &bridged] {
            let carried = swap_steps(stage, labels)
                .iter()
                .enumerate()
                .filter_map(|(index, step)| Some((index, step.order?)))
                .collect::<Vec<_>>();
            let expected = if has_order {
                vec![(1, uid)]
            } else {
                Vec::new()
            };
            assert_eq!(carried, expected, "{stage:?}");
        }
    }
}

#[test]
fn progress_offers_only_the_actions_the_order_state_allows() {
    for stage in every_stage() {
        let actions = swap_actions(stage, false);
        // Cancel only while the order is open and its pre-hook hasn't run.
        assert_eq!(
            actions.cancel.is_some(),
            matches!(
                stage,
                SwapStage::Order(SwapOrderState::Open)
                    | SwapStage::SubmissionPending
                    | SwapStage::SubmissionRejected
            ),
            "{stage:?}"
        );
        // Recovery for stranded or refunded funds. A retired account never held the swap's.
        // A bridge deposit that needs attention isn't in the account to recover.
        assert_eq!(
            actions.recover,
            matches!(
                stage,
                SwapStage::Order(
                    SwapOrderState::PreHookOnly { .. }
                        | SwapOrderState::NotDelivered
                        | SwapOrderState::Refunding
                )
            ),
            "{stage:?}"
        );
        // Held proceeds are recovered on the destination network, not from this account.
        assert_eq!(
            actions.recover_on_destination,
            stage.is_held_on_destination(),
            "{stage:?}"
        );
        // A retry is admitted only after the attempt ended; an executed pre-hook blocks it
        // with a reason.
        match stage {
            SwapStage::Order(SwapOrderState::AttemptEnded(_)) => {
                assert_eq!(actions.retry, Some(Ok(())));
            }
            SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }) => {
                assert!(matches!(actions.retry, Some(Err(_))));
            }
            _ => assert!(actions.retry.is_none(), "{stage:?}"),
        }
        // Only a stalled swap, one that can't move on by itself but hasn't ended, can be
        // dismissed.
        assert_eq!(
            actions.dismiss,
            stage.is_dismissible() && !stage.has_ended(),
            "{stage:?}"
        );
        let ongoing = matches!(
            stage,
            SwapStage::SetupSubmitting
                | SwapStage::SetupPending
                | SwapStage::SubmissionPending
                | SwapStage::SubmissionRejected
                | SwapStage::Order(
                    SwapOrderState::Open
                        | SwapOrderState::Traded
                        | SwapOrderState::Bridging
                        | SwapOrderState::NeedsAttention
                        | SwapOrderState::PreHookOnly { expired: false }
                )
        );
        assert_eq!(
            stage.is_shown_on_private_tab(true, false),
            ongoing,
            "{stage:?}"
        );
        // Ended swaps leave the Private tab by themselves; the rest stay until dismissed.
        assert_eq!(
            stage.is_shown_on_private_tab(false, false),
            !stage.has_ended(),
            "{stage:?}"
        );
        let needs_recovery = matches!(
            stage,
            SwapStage::Order(
                SwapOrderState::NotDelivered
                    | SwapOrderState::Refunding
                    | SwapOrderState::PreHookOnly { expired: true }
            )
        );
        assert_eq!(stage.needs_recovery(), needs_recovery, "{stage:?}");
        // A bridge deposit that needs attention asks the user to act, though not to recover.
        // Held proceeds ask for a recovery on the destination network.
        let needs_attention = needs_recovery
            || matches!(
                stage,
                SwapStage::Order(
                    SwapOrderState::NeedsAttention | SwapOrderState::HeldOnDestination
                )
            );
        assert_eq!(stage.needs_attention(), needs_attention, "{stage:?}");
        assert_eq!(
            swap_card_line(stage, &labels()).attention,
            needs_attention,
            "{stage:?}"
        );
        if matches!(
            stage,
            SwapStage::SubmissionPending | SwapStage::SubmissionRejected
        ) {
            assert_eq!(
                swap_steps(stage, &labels())[1].status,
                PublicActionStepStatus::Warning
            );
        }
    }
    assert!(
        swaps_card_line(&[
            (SwapStage::Order(SwapOrderState::Open), labels()),
            (SwapStage::Order(SwapOrderState::NotDelivered), labels()),
        ])
        .unwrap()
        .attention
    );
    // Private swaps alone show their own card whether or not Public-paid swaps exist
    // elsewhere. With one, the card keeps each private swap's pair and its attention count.
    let private = [
        (SwapStage::Order(SwapOrderState::Open), labels()),
        (SwapStage::Order(SwapOrderState::NotDelivered), labels()),
    ];
    for swaps in [&private[..1], &private[..]] {
        assert_eq!(card_line(swaps, None), swaps_card_line(swaps));
    }
    let incoming = || PublicCardLine {
        title: "Receiving USDC from Main on Ethereum".into(),
        detail: "Order open".into(),
        count: 1,
        attention: 0,
    };
    let pair = labels().pair;
    assert_eq!(
        card_line(&private[..1], Some(incoming())),
        Some(SwapCardLine {
            title: "2 swaps in progress".into(),
            detail: format!("{pair} · Receiving USDC from Main on Ethereum"),
            attention: false,
        })
    );
    assert_eq!(
        card_line(&private, Some(incoming())),
        Some(SwapCardLine {
            title: "3 swaps · 1 needs attention".into(),
            detail: format!("{pair} · {pair} · Receiving USDC from Main on Ethereum"),
            attention: true,
        })
    );
}

#[test]
fn orders_split_into_swaps_at_a_new_pair_or_after_a_trade() {
    let (weth, usdc, dai) = (
        Address::repeat_byte(1),
        Address::repeat_byte(2),
        Address::repeat_byte(3),
    );
    let reshield = || SwapDelivery::Reshield;
    let external = SwapDelivery::External {
        receiver: Address::repeat_byte(4),
    };
    let swap_use = SwapUseId::random().unwrap();
    // Each ordinary attempt has its own pre-hook nonce; a speed-up keeps the open order's.
    let funding = |nonce: u64| Some((swap_use, U256::from(nonce)));
    // An order-less record has no ranges; it is one swap without orders.
    assert_eq!(
        swap_order_ranges(std::iter::empty()),
        [] as [std::ops::Range<usize>; 0]
    );
    assert_eq!(
        swap_order_ranges([
            // Expired, then retried with the same pair: one swap.
            (weth, usdc, reshield(), false, funding(1)),
            (weth, usdc, reshield(), true, funding(2)),
            // After the trade, the same pair on the reused account is a new swap.
            (weth, usdc, reshield(), false, funding(3)),
            // A different pair is a new swap even after an attempt without a trade.
            (dai, usdc, reshield(), false, funding(4)),
            // So is another delivery for the same pair.
            (dai, usdc, external, false, funding(5)),
        ]),
        [0..2, 2..3, 3..4, 4..5]
    );
    // Two orders that compete for one funding are one swap whichever of them traded, and
    // an order funded on its own after that trade is the next swap, also for the same use.
    for (original, speed_up) in [(false, false), (true, false), (false, true)] {
        assert_eq!(
            swap_order_ranges([
                (weth, usdc, reshield(), original, funding(1)),
                (weth, usdc, reshield(), speed_up, funding(1)),
                (weth, usdc, reshield(), false, funding(2)),
            ]),
            if original || speed_up {
                vec![0..2, 2..3]
            } else {
                std::iter::once(0..3).collect()
            },
            "original traded: {original}, speed-up traded: {speed_up}"
        );
    }
}

#[test]
fn my_orders_groups_follow_what_the_swap_can_still_do() {
    use SwapOrderGroup::{Ended, NeedsAttention, NeedsRecovery, Open};
    for (stage, group) in [
        (SwapStage::SetupPending, Open),
        (SwapStage::Approved, Open),
        (SwapStage::SubmissionPending, Open),
        (SwapStage::Order(SwapOrderState::Open), Open),
        (
            SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }),
            Open,
        ),
        // Traded but not yet back in the private balance, or not yet delivered by the bridge.
        (SwapStage::Order(SwapOrderState::Traded), Open),
        (SwapStage::Order(SwapOrderState::Bridging), Open),
        // The provider has to resolve it, so it isn't grouped with recoverable swaps.
        (
            SwapStage::Order(SwapOrderState::NeedsAttention),
            NeedsAttention,
        ),
        (
            SwapStage::Order(SwapOrderState::PreHookOnly { expired: true }),
            NeedsRecovery,
        ),
        (
            SwapStage::Order(SwapOrderState::NotDelivered),
            NeedsRecovery,
        ),
        (SwapStage::Order(SwapOrderState::Refunding), NeedsRecovery),
        // The wallet can recover held proceeds, on the destination network.
        (
            SwapStage::Order(SwapOrderState::HeldOnDestination),
            NeedsRecovery,
        ),
        (SwapStage::Order(SwapOrderState::Done), Ended),
        (
            SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)),
            Ended,
        ),
        (
            SwapStage::Order(SwapOrderState::AttemptEnded(
                SwapPreHookDeathCause::Cancellation,
            )),
            Ended,
        ),
        (SwapStage::Recovered, Ended),
        (SwapStage::SetupRetired, Ended),
    ] {
        assert_eq!(swap_order_group(stage, false, false), group, "{stage:?}");
    }
    // A stopped setup has ended even while its sent setup may still confirm.
    assert_eq!(
        swap_order_group(SwapStage::SetupPending, true, false),
        Ended
    );
    for stage in every_stage() {
        // Removing a swap from the Private tab ends only swaps that can't move on by
        // themselves. It changes nothing for swaps in flight or needing recovery.
        let shown = swap_order_group(stage, false, false);
        let hidden = swap_order_group(stage, false, true);
        if stage.is_dismissible() && shown != NeedsRecovery {
            assert_eq!(hidden, Ended, "{stage:?}");
        } else {
            assert_eq!(hidden, shown, "{stage:?}");
        }
    }
}

#[test]
fn a_new_swap_is_quoted_before_setup_and_an_approved_one_waits_for_its_setup() {
    // The quote, the price check and the minimum come before any setup is paid, and the
    // review covers the setup. A setup sent again is reviewed the same way.
    assert_eq!(swap_form_mode(None), SwapFormMode::Setup { resume: false });
    for stage in [
        SwapStage::SetupNotSent,
        SwapStage::SetupFailed,
        SwapStage::SetupPending,
    ] {
        assert_eq!(
            swap_form_mode(Some(stage)),
            SwapFormMode::Setup { resume: true }
        );
    }
    // Pending setup can be retried with another broadcaster after the local job ends.
    assert_eq!(
        swap_form_mode(Some(SwapStage::SetupSubmitting)),
        SwapFormMode::SettingUp
    );
    assert_eq!(
        swap_actions(SwapStage::SetupPending, false).resume,
        Some(Ok(()))
    );
    assert!(SwapStage::SetupPending.is_observed());
    // Once the setup is confirmed, the approved order is placed through a confirm-only
    // step, and nothing is left to observe until it exists.
    let approved = swap_actions(SwapStage::Approved, false);
    assert_eq!(approved.resume, Some(Ok(())));
    assert!(approved.retry.is_none() && approved.cancel.is_none());
    assert!(!SwapStage::Approved.is_observed());
    // Changed terms, an amount that no longer fits, and retries go through the form
    // without a second setup.
    for stage in [
        SwapStage::Approved,
        SwapStage::Ready,
        SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)),
    ] {
        assert_eq!(
            swap_form_mode(Some(stage)),
            SwapFormMode::Order,
            "{stage:?}"
        );
    }
    for stage in [
        SwapStage::Order(SwapOrderState::Open),
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }),
        SwapStage::Recovered,
    ] {
        assert_eq!(
            swap_form_mode(Some(stage)),
            SwapFormMode::Placed,
            "{stage:?}"
        );
    }
}

#[test]
fn recovery_starts_with_the_token_the_stealth_account_holds() {
    use wallet_ops::vault::{BridgeDelivery, BridgeProvider, BridgeSurplus};
    let (sell, buy) = (Address::repeat_byte(1), Address::repeat_byte(2));
    let bridge = |provider, surplus| {
        SwapDelivery::Bridge(BridgeDelivery {
            provider,
            destination_chain: 42161,
            receiver: Address::repeat_byte(3),
            destination_token: Address::repeat_byte(4),
            surplus,
            private: None,
        })
    };
    let across = &bridge(BridgeProvider::Across, BridgeSurplus::KeepInAccount);
    let near = &bridge(
        BridgeProvider::NearIntents,
        BridgeSurplus::BridgedByProvider,
    );
    let order = SwapStage::Order;
    for (stage, delivery, token) in [
        (
            order(SwapOrderState::PreHookOnly { expired: true }),
            across,
            sell,
        ),
        // An Across post-hook that didn't run leaves the bought token in the account.
        (order(SwapOrderState::Traded), across, buy),
        (order(SwapOrderState::NotDelivered), across, buy),
        // NEAR Intents' trade pays its deposit address.
        (order(SwapOrderState::Traded), near, sell),
        // Either bridge refunds the bought token to the account.
        (order(SwapOrderState::Refunding), across, buy),
        (order(SwapOrderState::Refunding), near, buy),
    ] {
        assert_eq!(
            swap_recovery_token(stage, delivery, sell, buy),
            token,
            "{stage:?} {delivery:?}"
        );
    }
}

#[test]
fn local_expiry_hides_cancel_and_waits_for_a_final_expiry_before_retry() {
    let actions = swap_actions(SwapStage::Order(SwapOrderState::Open), true);
    assert!(actions.cancel.is_none());
    assert!(matches!(actions.retry, Some(Err(_))));
}

#[test]
fn done_requires_delivery_not_only_the_trade() {
    let traded = swap_steps(SwapStage::Order(SwapOrderState::Traded), &labels());
    assert_eq!(
        traded.last().map(|step| step.status),
        Some(PublicActionStepStatus::Pending)
    );
    let shield_transaction = Some(B256::repeat_byte(0x5e));
    let done = swap_steps(
        SwapStage::Order(SwapOrderState::Done),
        &SwapLabels {
            shield_transaction,
            ..labels()
        },
    );
    assert!(
        done.iter()
            .all(|step| step.status == PublicActionStepStatus::Done)
    );
    // The verified delivery keeps showing its shield transaction.
    assert_eq!(
        done.last().map(|step| step.transaction),
        Some(shield_transaction)
    );
    // A Public address swap's settlement delivers, so its delivery step shows it.
    let settlement_transaction = Some(B256::repeat_byte(0x5f));
    let external = swap_steps(
        SwapStage::Order(SwapOrderState::Done),
        &SwapLabels {
            receiver: Some("Treasury".into()),
            settlement_transaction,
            ..labels()
        },
    );
    assert_eq!(
        external
            .last()
            .map(|step| (step.label.as_str(), step.transaction)),
        Some(("Delivered to Treasury", settlement_transaction))
    );
    let undelivered = swap_steps(SwapStage::Order(SwapOrderState::NotDelivered), &labels());
    assert_ne!(
        undelivered.last().map(|step| step.status),
        Some(PublicActionStepStatus::Done)
    );
}

#[test]
fn bridge_steps_follow_the_hand_off_then_the_destination_outcome() {
    use PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
    let bridged = |provider, reported| SwapLabels {
        received: Some("248.71 USDC".into()),
        bridge: Some(SwapBridgeLabels {
            provider,
            network: "Polygon".into(),
            token: "USDC".into(),
            non_evm: false,
            origin: "Arbitrum One".into(),
            receiver: "Treasury".into(),
            sent: Some("248.82 USDC".into()),
            minimum: Some("248.70 USDC".into()),
            private: None,
            reported,
            delivered_transaction: None,
            destination_transaction: None,
            handoff_transaction: None,
            refund_transaction: None,
            deposit: SwapSeen::Looking,
            report: None,
        }),
        ..labels()
    };
    let (across, across_reported, near) = (
        bridged(BridgeProvider::Across, false),
        bridged(BridgeProvider::Across, true),
        bridged(BridgeProvider::NearIntents, true),
    );
    let order = SwapStage::Order;
    for (stage, labels, steps) in [
        // Before the trade, the hand-off and the delivery wait for it.
        (
            order(SwapOrderState::Open),
            &across,
            vec![
                ("Order open", Pending),
                ("Bridge deposit", NotStarted),
                ("Delivered on Polygon", NotStarted),
            ],
        ),
        // An Across trade to a public receiver without its deposit leaves the hand-off
        // unverified. Every other Bridge order still waits for its settlement to be read.
        (
            order(SwapOrderState::Traded),
            &across,
            vec![
                ("Order open", Done),
                ("Bridge deposit unverified", Pending),
                ("Delivered on Polygon", NotStarted),
            ],
        ),
        (
            order(SwapOrderState::Traded),
            &near,
            vec![
                ("Order open", Done),
                ("Bridge deposit", Pending),
                ("Delivered on Polygon", NotStarted),
            ],
        ),
        (
            order(SwapOrderState::Bridging),
            &across,
            vec![
                ("Order open", Done),
                ("Bridge deposit", Done),
                ("Delivered on Polygon", Pending),
            ],
        ),
        (
            order(SwapOrderState::Done),
            &across,
            vec![
                ("Order open", Done),
                ("Bridge deposit", Done),
                ("Delivered on Polygon · verified", Done),
            ],
        ),
        (
            order(SwapOrderState::Done),
            &near,
            vec![
                ("Order open", Done),
                ("Bridge deposit", Done),
                ("Delivered on Polygon · reported by NEAR Intents", Done),
            ],
        ),
        // Refunding and Needs attention replace the delivered step.
        (
            order(SwapOrderState::Refunding),
            &across,
            vec![
                ("Order open", Done),
                ("Bridge deposit", Done),
                ("Refunding on Arbitrum One", Warning),
            ],
        ),
        (
            order(SwapOrderState::NeedsAttention),
            &near,
            vec![
                ("Order open", Done),
                ("Bridge deposit", Done),
                ("Needs attention", Warning),
            ],
        ),
        // An Across post-hook that didn't run left the bought token on the swap's network.
        (
            order(SwapOrderState::NotDelivered),
            &across,
            vec![("Order open", Done), ("Not sent to the bridge", Warning)],
        ),
    ] {
        let shown = swap_steps(stage, labels);
        assert_eq!(
            shown[1..]
                .iter()
                .map(|step| (step.label.as_str(), step.status))
                .collect::<Vec<_>>(),
            steps,
            "{stage:?}"
        );
        // Nothing of a Bridge swap returns to the private balance by itself.
        let card = swap_card_line(stage, labels);
        assert!(
            shown
                .iter()
                .flat_map(|step| [&step.label, &step.detail])
                .chain([&card.title, &card.detail])
                .all(|text| !text.contains("private balance")),
            "{stage:?}"
        );
    }
    let pending = swap_steps(order(SwapOrderState::Bridging), &near);
    assert_eq!(pending[2].detail, "248.82 USDC to 1Click");
    assert!(
        pending[3]
            .detail
            .starts_with("At least 248.70 USDC to Treasury")
    );
    // The deposit step shows the recorded hand-off's transaction.
    let handoff = Some(B256::repeat_byte(0xd0));
    let mut recorded = across.clone();
    recorded.bridge.as_mut().unwrap().handoff_transaction = handoff;
    for state in [SwapOrderState::Bridging, SwapOrderState::Done] {
        let steps = swap_steps(order(state), &recorded);
        assert_eq!(
            (steps[2].label.as_str(), steps[2].transaction),
            (BRIDGE_DEPOSIT, handoff),
            "{state:?}"
        );
    }
    assert_eq!(
        swap_card_line(order(SwapOrderState::Done), &across).detail,
        "Delivered 248.71 USDC to Treasury on Polygon via Across"
    );
    assert_eq!(
        swap_card_line(order(SwapOrderState::Done), &across_reported).detail,
        "Delivered 248.71 USDC to Treasury on Polygon · reported by Across"
    );
    // A refund returns to the stealth account on the swap's own network.
    assert!(
        swap_card_line(order(SwapOrderState::Refunding), &across)
            .detail
            .contains("248.82 USDC returns to the stealth account on Arbitrum One")
    );
    // Needs attention is its own condition, which the card names, not a recovery.
    let attention = swap_card_line(order(SwapOrderState::NeedsAttention), &near);
    assert!(attention.attention && attention.detail.contains("Treasury on Polygon"));
    assert_eq!(
        swap_order_status(order(SwapOrderState::Done), false, &near),
        "Delivered"
    );
}

/// A non-EVM delivery's steps name its network and receiver as its record holds them. A
/// reported delivery carries 1Click's transaction in the network's own form, in place of
/// an EVM hash, and a failed deposit needs attention.
#[test]
fn non_evm_steps_carry_the_destination_transaction_as_reported() {
    use PublicActionStepStatus::{Done, Pending, Warning};
    const SIGNATURE: &str =
        "2zf1D3JpcScRk9TTCWSQmbKr8hB9nfiLK9iA7mHvNuxqSrDDLCRFY7mb48txGysNHWns8autYb6meqyhRAPs23qT";
    let solana = |destination_transaction: Option<&str>| SwapLabels {
        received: Some("181.25 JUP".into()),
        bridge: Some(SwapBridgeLabels {
            provider: BridgeProvider::NearIntents,
            network: "Solana".into(),
            token: "JUP".into(),
            non_evm: true,
            origin: "Ethereum".into(),
            receiver: "9jQtwH…Vkkv".into(),
            sent: Some("100.46 USDC".into()),
            minimum: Some("180.42 JUP".into()),
            private: None,
            reported: destination_transaction.is_some(),
            delivered_transaction: None,
            destination_transaction: destination_transaction.map(str::to_owned),
            handoff_transaction: None,
            refund_transaction: None,
            deposit: SwapSeen::Looking,
            report: None,
        }),
        ..labels()
    };
    let order = SwapStage::Order;
    let last = |stage, labels: &SwapLabels| swap_steps(order(stage), labels).pop().unwrap();
    let bridging = last(SwapOrderState::Bridging, &solana(None));
    assert_eq!(
        (bridging.label.as_str(), bridging.status),
        ("Delivered on Solana", Pending)
    );
    assert!(
        bridging
            .detail
            .starts_with("At least 180.42 JUP to 9jQtwH…Vkkv")
    );
    let delivered = last(SwapOrderState::Done, &solana(Some(SIGNATURE)));
    assert_eq!(
        (
            delivered.label.as_str(),
            delivered.detail.as_str(),
            delivered.status,
        ),
        (
            "Delivered on Solana · reported by NEAR Intents",
            "181.25 JUP to 9jQtwH…Vkkv",
            Done
        )
    );
    assert_eq!(
        (
            delivered.transaction,
            delivered.destination_transaction.as_deref()
        ),
        (None, Some(SIGNATURE))
    );
    let attention = last(SwapOrderState::NeedsAttention, &solana(None));
    assert_eq!(
        (attention.label.as_str(), attention.status),
        ("Needs attention", Warning)
    );
    assert_eq!(attention.destination_transaction, None);
}

#[test]
fn private_bridge_steps_follow_both_setups_then_the_shield_on_the_destination() {
    use PublicActionStepStatus::{Done, Error, NotStarted, Pending, Warning};
    use SwapSetupProgress as Setup;
    const RECEIVER: &str = "0x9a9A…9a9A";
    let destination = Address::repeat_byte(0x9a);
    let private = |origin: Setup, arrival: Setup| {
        let setup = |network: &str, index, address, progress| SwapSetupLabels {
            network: network.into(),
            account: Some(SwapStepAccount {
                index: Some(index),
                address,
            }),
            progress,
            reused: false,
            detail: None,
        };
        SwapLabels {
            received: Some("990.26 USDC".into()),
            bridge: Some(SwapBridgeLabels {
                provider: BridgeProvider::Across,
                network: "Arbitrum One".into(),
                token: "USDC".into(),
                non_evm: false,
                origin: "Ethereum".into(),
                receiver: RECEIVER.into(),
                sent: Some("993.05 USDC".into()),
                minimum: Some("990.12 USDC".into()),
                private: Some(SwapPrivateBridgeLabels {
                    setups: [
                        setup("Ethereum", 191, Address::repeat_byte(0x4e), origin),
                        setup("Arbitrum One", 57, destination, arrival),
                    ],
                    held: Some("992.74 USDC".into()),
                }),
                reported: false,
                delivered_transaction: None,
                destination_transaction: None,
                handoff_transaction: None,
                refund_transaction: None,
                deposit: SwapSeen::Looking,
                report: None,
            }),
            ..labels()
        }
    };
    let children = |step: &SwapStep| {
        step.children
            .iter()
            .map(|child| (child.label.clone(), child.status, child.detail.clone()))
            .collect::<Vec<_>>()
    };
    let child =
        |network: &str, status, detail: &str| (network.to_owned(), status, detail.to_owned());

    // One setup confirmed, one on its way: the step is in progress and counts on its
    // sub-steps, each with its own account.
    let pending = swap_steps(
        SwapStage::SetupPending,
        &private(Setup::Done, Setup::Pending),
    );
    assert_eq!(
        pending
            .iter()
            .map(|step| (step.label.as_str(), step.status))
            .collect::<Vec<_>>(),
        [
            ("Stealth accounts set up", Pending),
            ("Order open", NotStarted),
            ("Bridge deposit", NotStarted),
            ("Private on Arbitrum One", NotStarted),
        ]
    );
    assert_eq!(
        children(&pending[0]),
        [
            child("Ethereum", Done, ""),
            child("Arbitrum One", Pending, "Waiting for broadcaster…"),
        ]
    );
    assert_eq!(
        pending[0].children[1].account,
        Some(SwapStepAccount {
            index: Some(57),
            address: destination
        })
    );
    // The parent stays in progress while either network still needs its setup.
    let step = |stage, origin, arrival| swap_steps(stage, &private(origin, arrival)).remove(0);
    let loading = step(SwapStage::SetupPending, Setup::Done, Setup::NetworkLoading);
    assert_eq!(loading.status, Pending);
    assert_eq!(
        loading.children[1].detail,
        "Waiting for Arbitrum One to load…"
    );
    assert_eq!(
        step(SwapStage::SetupNotSent, Setup::NotSent, Setup::NotSent).status,
        Pending
    );

    // One failed setup fails the step, whatever the other did, and no order follows.
    let failed = swap_steps(SwapStage::SetupFailed, &private(Setup::Done, Setup::Failed));
    assert_eq!(failed.len(), 2);
    assert_eq!(failed[0].status, Error);
    assert!(failed[0].detail.contains("Arbitrum One"));
    assert_eq!(
        children(&failed[0]),
        [
            child("Ethereum", Done, ""),
            child("Arbitrum One", Error, "Not confirmed"),
        ]
    );
    assert_eq!(
        step(SwapStage::SetupFailed, Setup::Failed, Setup::Pending).status,
        Error
    );

    // An account the swap reuses is ready without a setup of its own. Only the new
    // account's setup shows as pending, and the step waits for it.
    let reuse = |mut labels: SwapLabels, origin: bool, arrival: bool| {
        let bridge = labels.bridge.as_mut().unwrap();
        let setups = &mut bridge.private.as_mut().unwrap().setups;
        setups[0].reused = origin;
        setups[1].reused = arrival;
        labels
    };
    let mixed = swap_steps(
        SwapStage::SetupPending,
        &reuse(private(Setup::Done, Setup::Pending), true, false),
    );
    assert_eq!(
        (mixed[0].label.as_str(), mixed[0].status),
        ("Stealth accounts set up", Pending)
    );
    assert_eq!(
        children(&mixed[0]),
        [
            child("Ethereum", Done, "Reused · ready"),
            child("Arbitrum One", Pending, "Waiting for broadcaster…"),
        ]
    );
    // With both reused nothing is set up: the step is done and the order is next.
    let both = swap_steps(
        SwapStage::Ready,
        &reuse(private(Setup::Done, Setup::Done), true, true),
    );
    assert_eq!(
        both.iter()
            .take(2)
            .map(|step| (step.label.as_str(), step.status))
            .collect::<Vec<_>>(),
        [("Stealth accounts ready", Done), ("Order open", NotStarted)]
    );
    assert_eq!(
        children(&both[0]),
        [
            child("Ethereum", Done, "Reused · ready"),
            child("Arbitrum One", Done, "Reused · ready"),
        ]
    );
    // A reused account whose network isn't loaded shows no pending marker.
    let unloaded = swap_steps(
        SwapStage::Ready,
        &reuse(private(Setup::Done, Setup::NetworkLoading), true, true),
    );
    assert_eq!(
        children(&unloaded[0])[1],
        child(
            "Arbitrum One",
            NotStarted,
            "Reused · checked once Arbitrum One loads"
        )
    );

    // With both set up, the last step is the shield on the destination network, or what
    // replaced it.
    let set_up = private(Setup::Done, Setup::Done);
    let order = SwapStage::Order;
    for (state, last, detail) in [
        (
            SwapOrderState::Bridging,
            ("Private on Arbitrum One", Pending),
            "990.12 USDC to your private balance",
        ),
        (
            SwapOrderState::Done,
            ("Private on Arbitrum One · verified", Done),
            "990.26 USDC shielded to your private balance",
        ),
        (
            SwapOrderState::HeldOnDestination,
            ("Held by stealth account on Arbitrum One", Warning),
            "Across delivered 992.74 USDC, but the shield didn't run. The USDC is in stealth account #57 on Arbitrum One.",
        ),
        (
            SwapOrderState::Refunding,
            ("Refunding on Ethereum", Warning),
            "No relayer filled the deposit",
        ),
    ] {
        let steps = swap_steps(order(state), &set_up);
        assert_eq!(steps[0].status, Done, "{state:?}");
        assert_eq!(
            children(&steps[0]),
            [child("Ethereum", Done, ""), child("Arbitrum One", Done, ""),],
            "{state:?}"
        );
        let shown = steps.last().unwrap();
        assert_eq!((shown.label.as_str(), shown.status), last, "{state:?}");
        assert!(shown.detail.starts_with(detail), "{}", shown.detail);
    }
    assert_eq!(
        swap_card_line(order(SwapOrderState::Bridging), &set_up).detail,
        "Sent to Across · shielding to your private balance on Arbitrum One"
    );
    // Held proceeds are the wallet's to recover, on the destination network.
    let held = order(SwapOrderState::HeldOnDestination);
    for hidden in [false, true] {
        assert_eq!(
            swap_order_group(held, false, hidden),
            SwapOrderGroup::NeedsRecovery
        );
    }
    assert_eq!(
        swap_order_status(held, false, &set_up),
        "Held on Arbitrum One"
    );
    let card = swap_card_line(held, &set_up);
    assert!(card.attention && card.detail.contains("992.74 USDC"));

    // The destination stealth account is never named as a receiver.
    for stage in every_stage() {
        let steps = swap_steps(stage, &set_up);
        let card = swap_card_line(stage, &set_up);
        assert!(
            steps
                .iter()
                .flat_map(|step| std::iter::once(step).chain(&step.children))
                .flat_map(|step| [&step.label, &step.detail])
                .chain([&card.title, &card.detail])
                .all(|text| !text.contains(RECEIVER)),
            "{stage:?}"
        );
    }
}

/// A fill the orderbook reports 5 blocks after the trade, with the bought amount, whose
/// block wasn't read yet.
fn fill_hint() -> SwapFillHint {
    SwapFillHint {
        confirmations: Some(5),
        depth: 12,
        bought: Some("49.40 USDC".into()),
        detected: false,
        shield: SwapSeen::Looking,
        shield_transaction: None,
        origin: "Ethereum".into(),
    }
}

/// A swap delivered by `provider` to Treasury on Polygon, with `report` its provider's word
/// on the delivery.
fn bridged(provider: BridgeProvider, report: Option<SwapBridgeReported>) -> SwapLabels {
    SwapLabels {
        bridge: Some(SwapBridgeLabels {
            provider,
            network: "Polygon".into(),
            token: "USDC".into(),
            non_evm: false,
            origin: "Ethereum".into(),
            receiver: "Treasury".into(),
            sent: None,
            minimum: Some("248.70 USDC".into()),
            private: None,
            reported: false,
            delivered_transaction: None,
            destination_transaction: None,
            handoff_transaction: None,
            refund_transaction: None,
            deposit: SwapSeen::Looking,
            report,
        }),
        ..labels()
    }
}

#[test]
fn a_reported_fill_shows_the_trade_confirming_until_observation_records_it() {
    use PublicActionStepStatus::{Done, Pending};
    // The orderbook reports the fill 5 blocks after the trade, and even past the local
    // expiry, since a fill can land up to validTo.
    let mut hinted = SwapLabels {
        fill_hint: Some(fill_hint()),
        lapsed: true,
        ..labels()
    };
    let stages = [
        SwapStage::Order(SwapOrderState::Open),
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }),
    ];
    for stage in stages {
        for (confirmations, progress) in [
            (Some(5), "Confirming (5/12)"),
            (Some(12), "Verifying settlement…"),
            (Some(20), "Verifying settlement…"),
            (None, "Confirming"),
        ] {
            hinted.fill_hint.as_mut().unwrap().confirmations = confirmations;
            let steps = swap_steps(stage, &hinted);
            assert_eq!(
                steps.iter().map(|step| step.status).collect::<Vec<_>>(),
                [Done, Done, Pending, Pending],
                "the settlement's steps stay pending until canonical verification: {stage:?}"
            );
            // The amount is the order's, so it reads as bought, never as received.
            assert_eq!(
                (steps[2].label.as_str(), steps[2].detail.as_str()),
                (
                    "Traded",
                    format!("{progress} · CoW reports 49.40 USDC bought").as_str()
                )
            );
            // The private return confirms with the trade and repeats no count.
            assert_eq!(
                (steps[3].label.as_str(), steps[3].detail.as_str()),
                ("Back in private balance", "Looking for the shield…")
            );
        }
        let card = swap_card_line(stage, &hinted);
        assert_eq!(
            (card.title.as_str(), card.detail.as_str()),
            ("Swapped 1.00 WETH for USDC", "Traded · confirming")
        );
        assert_eq!(
            reported_fill_note(stage, &hinted).as_deref(),
            Some("Nothing more to do. The result is confirmed after 12 blocks on Ethereum.")
        );
    }
    // Once the reported block was read, the trade's amount is the chain's, and the private
    // return tells whether the block shows the shield. The trade stays pending, and a
    // shield the block shows is done, with its transaction.
    let shield_transaction = Some(B256::repeat_byte(0x5e));
    for (shield, transaction, status, detail) in [
        (
            SwapSeen::Detected(Some("49.28 USDC".into())),
            shield_transaction,
            Done,
            "Shield detected · 49.28 USDC",
        ),
        (
            SwapSeen::Detected(None),
            shield_transaction,
            Done,
            "Shield detected",
        ),
        (SwapSeen::NotFound, None, Pending, "No shield detected yet"),
    ] {
        hinted.fill_hint = Some(SwapFillHint {
            detected: true,
            shield,
            shield_transaction: transaction,
            ..fill_hint()
        });
        let steps = swap_steps(stages[0], &hinted);
        assert_eq!(
            steps[2..]
                .iter()
                .map(|step| (step.status, step.detail.as_str(), step.transaction))
                .collect::<Vec<_>>(),
            [
                (Pending, "Confirming (5/12) · 49.40 USDC bought", None),
                (status, detail, transaction),
            ]
        );
    }
    // Without an amount the trade shows its count alone.
    hinted.fill_hint = Some(SwapFillHint {
        bought: None,
        ..fill_hint()
    });
    assert_eq!(
        swap_steps(stages[0], &hinted)[2].detail,
        "Confirming (5/12)"
    );

    // A Public address swap's one delivery step confirms with the trade's count and amount.
    let external = SwapLabels {
        fill_hint: Some(fill_hint()),
        receiver: Some("Treasury".into()),
        minimum: Some("48.90 USDC".into()),
        ..labels()
    };
    let steps = swap_steps(stages[0], &external);
    assert_eq!(
        steps
            .iter()
            .map(|step| (step.label.as_str(), step.status))
            .collect::<Vec<_>>(),
        [
            ("Stealth account set up", Done),
            ("Order open", Done),
            ("Delivered to Treasury", Pending),
        ]
    );
    assert_eq!(
        steps[2].detail,
        "Confirming (5/12) · CoW reports 49.40 USDC bought"
    );
}

#[test]
fn a_reported_bridge_fill_confirms_the_deposit_and_shows_the_providers_report() {
    use PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
    let open = SwapStage::Order(SwapOrderState::Open);
    let hinted = |provider, report| SwapLabels {
        fill_hint: Some(fill_hint()),
        ..bridged(provider, report)
    };
    let fill = Some(B256::repeat_byte(0xf1));
    let delivered = |amount: Option<&str>| SwapBridgeReported::Delivered {
        amount: amount.map(Into::into),
        shield: SwapShieldSeen::Looking,
        transaction: fill,
    };
    for (provider, name, expected, in_transit) in [
        (
            BridgeProvider::Across,
            "Across",
            "248.70 USDC to Treasury",
            "248.70 USDC to Treasury · Across usually fills within a few minutes",
        ),
        (
            BridgeProvider::NearIntents,
            "NEAR Intents",
            "At least 248.70 USDC to Treasury",
            "At least 248.70 USDC to Treasury · NEAR Intents usually delivers within a few minutes",
        ),
    ] {
        for (report, label, status, detail) in [
            (
                None,
                "Delivered on Polygon".to_owned(),
                NotStarted,
                expected,
            ),
            (
                Some(SwapBridgeReported::Pending),
                "Delivered on Polygon".to_owned(),
                Pending,
                in_transit,
            ),
            (
                Some(delivered(Some("248.71 USDC"))),
                format!("Delivered on Polygon · reported by {name}"),
                Done,
                "248.71 USDC to Treasury",
            ),
            (
                Some(delivered(None)),
                format!("Delivered on Polygon · reported by {name}"),
                Done,
                "To Treasury",
            ),
            (
                Some(SwapBridgeReported::Refunded),
                format!("Refund · reported by {name}"),
                Warning,
                "reports the deposit refunded or expired",
            ),
            (
                Some(SwapBridgeReported::Failed),
                format!("Problem · reported by {name}"),
                Warning,
                "reports the deposit as failed or incomplete",
            ),
        ] {
            let labels = hinted(provider, report.clone());
            let steps = swap_steps(open, &labels);
            // The trade and the deposit confirm together, with the count on the trade.
            assert_eq!(
                steps[1..4]
                    .iter()
                    .map(|step| (step.label.as_str(), step.status, step.detail.as_str()))
                    .collect::<Vec<_>>(),
                [
                    ("Order open", Done, ""),
                    (
                        "Traded",
                        Pending,
                        "Confirming (5/12) · CoW reports 49.40 USDC bought"
                    ),
                    ("Bridge deposit", Pending, "Looking for the deposit…"),
                ],
                "{provider:?} {report:?}"
            );
            assert_eq!(steps.len(), 5);
            assert_eq!(
                (steps[4].label.as_str(), steps[4].status),
                (label.as_str(), status),
                "{provider:?} {report:?}"
            );
            assert!(steps[4].detail.contains(detail), "{}", steps[4].detail);
            // Only a reported delivery reads as delivered, and only a reported refund or
            // failure changes the note.
            let made = matches!(report, Some(SwapBridgeReported::Delivered { .. }));
            // A reported delivery's step carries the provider's fill transaction.
            assert_eq!(
                steps[4].transaction,
                if made { fill } else { None },
                "{provider:?} {report:?}"
            );
            let card = swap_card_line(open, &labels);
            assert_eq!(card.title, "Swapped 1.00 WETH for USDC");
            assert_eq!(
                card.detail,
                if made {
                    "Delivered to Treasury on Polygon · confirming"
                } else {
                    "Traded · confirming"
                }
            );
            assert_eq!(
                swap_order_status(open, false, &labels),
                if made {
                    "Delivered · confirming"
                } else {
                    "Traded · confirming"
                }
            );
            let problem = matches!(
                report,
                Some(SwapBridgeReported::Refunded | SwapBridgeReported::Failed)
            );
            assert_eq!(
                reported_fill_note(open, &labels).as_deref(),
                Some(if problem {
                    "The wallet acts on this once the result is confirmed, after 12 blocks on Ethereum."
                } else {
                    "Nothing more to do. The result is confirmed after 12 blocks on Ethereum."
                })
            );
        }
    }

    // Once the reported block was read, the deposit tells whether the block shows it, named
    // after the provider's own deposit, and stays pending.
    for (provider, deposit, detail) in [
        (
            BridgeProvider::Across,
            SwapSeen::Detected(Some("49.40 USDC".into())),
            "Deposit detected · 49.40 USDC to Across",
        ),
        (
            BridgeProvider::NearIntents,
            SwapSeen::Detected(Some("49.40 USDC".into())),
            "Deposit detected · 49.40 USDC to 1Click",
        ),
        (
            BridgeProvider::Across,
            SwapSeen::Detected(None),
            "Deposit detected",
        ),
        (
            BridgeProvider::Across,
            SwapSeen::NotFound,
            "No deposit detected yet",
        ),
    ] {
        let mut labels = hinted(provider, None);
        labels.bridge.as_mut().unwrap().deposit = deposit;
        let step = &swap_steps(open, &labels)[3];
        assert_eq!(
            (step.label.as_str(), step.status, step.detail.as_str()),
            ("Bridge deposit", Pending, detail)
        );
    }

    // A private delivery's report covers the bridge alone, so its last step follows what
    // the fill block on Polygon shows. Nothing says the proceeds are in the private balance
    // before that block shows the shield, and proceeds the stealth account holds read as a
    // problem.
    let nothing_to_do = "Nothing more to do. The result is confirmed after 12 blocks on Ethereum.";
    for (shield, last, card, status, note) in [
        (
            SwapShieldSeen::Looking,
            (
                "Delivered on Polygon · reported by Across",
                Pending,
                "992.74 USDC delivered · looking for the shield…",
            ),
            "Delivered on Polygon · confirming",
            "Traded · confirming",
            nothing_to_do,
        ),
        (
            SwapShieldSeen::Detected("990.26 USDC".into()),
            ("Private on Polygon", Done, "Shield detected · 990.26 USDC"),
            "Shielded to your private balance on Polygon · confirming",
            "Delivered · confirming",
            nothing_to_do,
        ),
        (
            SwapShieldSeen::Held("992.74 USDC".into()),
            (
                "Held by stealth account on Polygon",
                Warning,
                "Across delivered 992.74 USDC, but the shield didn't run. The USDC is in the stealth account on Polygon.",
            ),
            "Traded · confirming",
            "Traded · confirming",
            "The wallet acts on this once the result is confirmed, after 12 blocks on Ethereum.",
        ),
    ] {
        let mut private = hinted(
            BridgeProvider::Across,
            Some(SwapBridgeReported::Delivered {
                amount: Some("992.74 USDC".into()),
                shield,
                transaction: fill,
            }),
        );
        private.bridge.as_mut().unwrap().private = Some(SwapPrivateBridgeLabels {
            setups: ["Ethereum", "Polygon"].map(|network| SwapSetupLabels {
                network: network.into(),
                account: None,
                progress: SwapSetupProgress::Done,
                reused: false,
                detail: None,
            }),
            held: None,
        });
        let steps = swap_steps(open, &private);
        let step = steps.last().unwrap();
        assert_eq!(
            (step.label.as_str(), step.status, step.detail.as_str()),
            last
        );
        assert_eq!(step.transaction, fill);
        assert_eq!(swap_card_line(open, &private).detail, card);
        assert_eq!(swap_order_status(open, false, &private), status);
        assert_eq!(reported_fill_note(open, &private).as_deref(), Some(note));
    }

    // Once observation records the delivery, its step carries the recorded fill.
    let mut recorded = bridged(BridgeProvider::Across, None);
    recorded.bridge.as_mut().unwrap().delivered_transaction = fill;
    let steps = swap_steps(SwapStage::Order(SwapOrderState::Done), &recorded);
    let step = steps.last().unwrap();
    assert_eq!(
        (step.label.as_str(), step.transaction),
        ("Delivered on Polygon · verified", fill)
    );
}

#[test]
fn a_report_that_is_not_borne_out_leaves_the_observed_steps() {
    use PublicActionStepStatus::{Done, NotStarted, Pending};
    // Verification found no trade, so the fill hint is gone. Each shape of swap shows its
    // observed stage again, and a deposit read early or a provider's report left behind
    // shows nothing by itself.
    let left = |provider| {
        let mut labels = bridged(
            provider,
            Some(SwapBridgeReported::Delivered {
                amount: Some("248.71 USDC".into()),
                shield: SwapShieldSeen::Looking,
                transaction: None,
            }),
        );
        labels.bridge.as_mut().unwrap().deposit = SwapSeen::Detected(Some("49.40 USDC".into()));
        labels
    };
    let open = SwapStage::Order(SwapOrderState::Open);
    let waiting = [
        ("Bridge deposit", NotStarted),
        ("Delivered on Polygon", NotStarted),
    ];
    for (labels, last) in [
        (
            labels(),
            vec![
                ("Traded", NotStarted),
                ("Back in private balance", NotStarted),
            ],
        ),
        (
            SwapLabels {
                receiver: Some("Treasury".into()),
                ..labels()
            },
            vec![("Delivered to Treasury", NotStarted)],
        ),
        (left(BridgeProvider::Across), waiting.to_vec()),
        (left(BridgeProvider::NearIntents), waiting.to_vec()),
    ] {
        let steps = swap_steps(open, &labels);
        assert_eq!(
            steps
                .iter()
                .map(|step| (step.label.as_str(), step.status))
                .collect::<Vec<_>>(),
            [
                vec![("Stealth account set up", Done), ("Order open", Pending)],
                last
            ]
            .concat()
        );
        // No step keeps a count, a reported amount, anything detected early or its tie to
        // a trade.
        assert!(
            steps
                .iter()
                .all(|step| ["Confirm", "report", "detected", "Looking"]
                    .iter()
                    .all(|text| !step.detail.contains(text))),
            "{steps:?}"
        );
        let card = swap_card_line(open, &labels);
        assert_eq!(
            (card.title.as_str(), card.detail.as_str()),
            ("Swapping 1.00 WETH for USDC", "Order open · expires 14:32")
        );
        assert_eq!(reported_fill_note(open, &labels), None);
    }
}

#[test]
fn a_traded_order_reports_its_outcome_against_the_approved_bounds() {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::SwapObservation;
    // The first mainnet fill: 50 USDT spent privately, 49.875 USDT sold after the unshield
    // fee, for 49.4041 DAI against a 42.6174 DAI minimum.
    let bounds = SwapApprovedBounds {
        sell_amount: U256::from(49_875_000u64),
        unshield_amount: Some(U256::from(50_000_000u64)),
        unshield_fee_bps: U256::from(25u8),
        buy_amount: U256::from(42_617_400_000_000_000_000u128),
        private_minimum: U256::from(42_510_856_500_000_000_000u128),
        shield_fee_bps: U256::from(25u8),
        slippage_bps: 50,
        pre_hook_gas_limit: 900_000,
        post_hook_gas_limit: Some(300_000),
        hook_cost: None,
        anchors: Vec::new(),
        destination_minimum: None,
        gas_share_bps: None,
        gas_estimate: None,
        gas_allowance: None,
        gas_price_wei: None,
        valid_for_secs: None,
        destination_shield_fee_bps: None,
        delivery_allowance: None,
        destination_setup_fee: None,
        source_setup_fee: None,
    };
    let settlement = B256::repeat_byte(0x51);
    let observed = SwapOrderObservations {
        traded: Some(SwapObservation {
            block: BlockNumHash::new(26_063_930, B256::repeat_byte(1)),
            transaction_hash: Some(settlement),
        }),
        trade_amounts: Some(SwapTradeAmounts {
            sell_amount: U256::from(49_875_000u64),
            buy_amount: U256::from(49_404_100_000_000_000_000u128),
            fee_amount: U256::ZERO,
            settlement_gas_used: None,
            settlement_effective_gas_price: None,
            executed_fee: None,
            executed_fee_token: None,
        }),
        ..SwapOrderObservations::default()
    };
    let reshield = SwapDelivery::Reshield;
    let outcome = swap_outcome(&bounds, &observed, &reshield).expect("traded");
    assert_eq!(
        (outcome.spent, outcome.unshield_fee),
        (U256::from(50_000_000u64), U256::from(125_000u64))
    );
    assert_eq!(outcome.settlement, Some(settlement));
    // Private delivery is measured by its credit, which isn't recorded yet.
    assert_eq!((outcome.received, outcome.above_minimum), (None, None));
    // A Public address receiver got the whole trade: 6.8932435 DAI above its 42.5108565
    // DAI minimum.
    let external = SwapDelivery::External {
        receiver: Address::repeat_byte(7),
    };
    let paid = swap_outcome(&bounds, &observed, &external).expect("traded");
    assert_eq!(
        (paid.private_minimum, paid.received, paid.above_minimum),
        (
            bounds.private_minimum,
            Some(U256::from(49_404_100_000_000_000_000u128)),
            Some(U256::from(6_893_243_500_000_000_000u128))
        )
    );
    // The credit after the shield fee, 49.28058975 DAI, against the same minimum.
    let credited = swap_outcome(
        &bounds,
        &SwapOrderObservations {
            shielded: Some(
                serde_json::from_value(serde_json::json!({
                    "observation": observed.traded.expect("traded"),
                    "private_amount": U256::from(49_280_589_750_000_000_000u128),
                    "fee": U256::from(123_510_250_000_000_000u128),
                }))
                .unwrap(),
            ),
            ..observed.clone()
        },
        &reshield,
    )
    .expect("traded");
    assert_eq!(
        (credited.received, credited.above_minimum),
        (
            Some(U256::from(49_280_589_750_000_000_000u128)),
            Some(U256::from(6_769_733_250_000_000_000u128))
        )
    );
    // The fee row needs the orderbook's fee and the settlement receipt's cost, all four
    // figures; any one missing leaves it out.
    let dai = Address::repeat_byte(0xda);
    let charged = SwapTradeAmounts {
        settlement_gas_used: Some(250_000),
        settlement_effective_gas_price: Some(2_000_000_000),
        executed_fee: Some(U256::from(100_000_000_000_000_000u128)),
        executed_fee_token: Some(dai),
        ..observed.trade_amounts.expect("amounts")
    };
    let gas = |amounts| {
        swap_outcome(
            &bounds,
            &SwapOrderObservations {
                trade_amounts: Some(amounts),
                ..observed.clone()
            },
            &reshield,
        )
        .expect("traded")
        .gas
    };
    assert_eq!(
        gas(charged),
        Some(SwapOutcomeGas {
            fee: U256::from(100_000_000_000_000_000u128),
            fee_token: dai,
            settlement_cost: U256::from(500_000_000_000_000u128),
        })
    );
    for partial in [
        SwapTradeAmounts {
            executed_fee: None,
            ..charged
        },
        SwapTradeAmounts {
            executed_fee_token: None,
            ..charged
        },
        SwapTradeAmounts {
            settlement_gas_used: None,
            ..charged
        },
        SwapTradeAmounts {
            settlement_effective_gas_price: None,
            ..charged
        },
    ] {
        assert_eq!(gas(partial), None);
    }
    // The shield fee is the one its event charged, not derived from the approved rate
    // (997_899 at 25 bps would give 2_501). A shield recorded without one shows no fee.
    let received = |fee: Option<U256>| {
        let shield: wallet_ops::vault::SwapShieldObservation =
            serde_json::from_value(serde_json::json!({
                "observation": observed.traded.expect("traded"),
                "private_amount": U256::from(997_899u64),
                "fee": fee,
            }))
            .unwrap();
        swap_outcome(
            &bounds,
            &SwapOrderObservations {
                shielded: Some(shield),
                ..observed.clone()
            },
            &reshield,
        )
        .expect("traded")
        .received_privately
    };
    assert_eq!(
        received(Some(U256::from(2_500u64))),
        Some((U256::from(997_899u64), Some(U256::from(2_500u64))))
    );
    assert_eq!(received(None), Some((U256::from(997_899u64), None)));
    // A record from before amounts were kept still shows the approved limit.
    let legacy = swap_outcome(
        &bounds,
        &SwapOrderObservations {
            trade_amounts: None,
            ..observed
        },
        &external,
    )
    .expect("traded");
    assert_eq!(
        (legacy.trade, legacy.received, legacy.gas),
        (None, None, None)
    );
    assert_eq!(legacy.minimum, bounds.buy_amount);
    assert_eq!(
        swap_outcome(&bounds, &SwapOrderObservations::default(), &reshield),
        None
    );
}

#[test]
fn nothing_unshielded_is_claimed_only_for_attempts_whose_pre_hook_never_ran() {
    for stage in every_stage() {
        let claims = swap_card_line(stage, &labels())
            .detail
            .contains("Nothing was unshielded")
            || swap_steps(stage, &labels())
                .iter()
                .any(|step| step.detail.contains("Nothing was unshielded"));
        let never_ran = matches!(
            stage,
            SwapStage::SetupFailed | SwapStage::Order(SwapOrderState::AttemptEnded(_))
        );
        assert_eq!(claims, never_ran, "{stage:?}");
    }
}

#[test]
fn a_recorded_setup_state_stands_in_for_an_unobserved_session() {
    use ExecutorPayloadState::{Pending, Resolved};
    assert_eq!(recorded_setup_stage(&[], false), SwapStage::SetupNotSent);
    // Two setups signed at one nonce, which is consumed.
    assert_eq!(
        recorded_setup_stage(&[Resolved, Resolved], false),
        SwapStage::Ready
    );
    // This session confirms the delegation itself before offering the order.
    assert_eq!(
        recorded_setup_stage(&[Resolved], true),
        SwapStage::SetupPending
    );
    // An unconsumed nonce stays pending and retryable, whatever a transaction did.
    assert_eq!(
        recorded_setup_stage(&[Pending, Pending], false),
        SwapStage::SetupPending
    );
}

#[test]
fn observation_pages_stay_within_the_confirmed_block_and_the_page_limit() {
    assert_eq!(swap_observation_range(100, 1_000), 100..164);
    assert_eq!(swap_observation_range(990, 1_000), 990..1_001);
    // Caught up, or a cursor past a lagging head: reread the confirmed block.
    assert_eq!(swap_observation_range(1_001, 1_000), 1_000..1_001);
    assert_eq!(swap_observation_range(5_000, 1_000), 1_000..1_001);
}

#[test]
fn total_cost_includes_cow_and_railgun_fees_across_token_decimals() {
    // Spend 1 WETH: 0.0025 WETH to unshield and 0.1 WETH to CoW. The remaining
    // 0.8975 WETH trades at 2,500 USDC/WETH, paying 2,243.75 USDC. Subtracting
    // 10 USDC for hooks and 5.584375 USDC to shield leaves 2,228.165625 USDC.
    let received = U256::from(2_228_165_625u64);
    let cost = swap_total_cost(
        U256::from(102_500_000_000_000_000u64),
        U256::from(897_500_000_000_000_000u64),
        U256::from(2_243_750_000u64),
        received,
    );
    assert_eq!(cost, U256::from(271_834_375u64));
    assert_eq!(cost + received, U256::from(2_500_000_000u64));
    assert_eq!(swap_cost_bps(cost, received), 1_087);
    // The percentage is a share of the input, not of the smaller amount received.
    assert_eq!(swap_cost_bps(U256::from(999), U256::from(9_001)), 999);
    assert_eq!(swap_cost_bps(U256::from(1_000), U256::from(9_000)), 1_000);
    // Large token supplies must not overflow while converting the ratio to basis points.
    assert_eq!(swap_cost_bps(U256::MAX, U256::MAX), 5_000);
}

#[test]
fn price_check_delta_is_signed_and_formatted_in_percent() {
    let expected = U256::from(10_000u32);
    assert_eq!(
        quote_anchor_delta_bps(U256::from(9_960u32), expected),
        Some(-40)
    );
    assert_eq!(
        quote_anchor_delta_bps(U256::from(10_125u32), expected),
        Some(125)
    );
    assert_eq!(quote_anchor_delta_bps(U256::ZERO, expected), None);
    assert_eq!(format_bps_percent(40), "0.4%");
    assert_eq!(format_bps_percent(125), "1.25%");
    assert_eq!(format_bps_percent(300), "3%");
}

pub(in crate::root::private_swap) fn public_presentation_record(
    order: bool,
    observations: &wallet_ops::vault::PublicSwapObservations,
) -> ExecutorRecord {
    use wallet_ops::vault::{PublicSwapApproval, PublicSwapIntent, PublicSwapRecord};
    let bounds: SwapApprovedBounds = serde_json::from_value(serde_json::json!({
        "sell_amount": "0x3e8", "buy_amount": "0x3e1", "private_minimum": "0x3dc",
        "shield_fee_bps": "0x0", "slippage_bps": 50, "pre_hook_gas_limit": 0, "anchors": []
    }))
    .unwrap();
    let swap = PublicSwapRecord::new(
        PublicSwapApproval {
            bounds,
            price_verified: Some(true),
            price_acknowledged: false,
            sell_token: Address::repeat_byte(1),
            on_shield_failure: wallet_ops::vault::BridgeShieldFailure::RefundOnOrigin,
            destination: wallet_ops::vault::SwapApprovedAccount {
                address: Some(Address::repeat_byte(3)),
                setup: false,
            },
            max_gas_cost: U256::ZERO,
        },
        PublicSwapIntent {
            bridged_token: Address::repeat_byte(2),
            order,
        },
    );
    let mut swap = serde_json::to_value(swap).unwrap();
    swap["observations"] = serde_json::to_value(observations).unwrap();
    if order {
        swap["path"] = serde_json::json!({ "Order": {
            "uid": alloy::hex::encode_prefixed([0u8; 56]),
            "buy_token": Address::repeat_byte(2), "proxy": Address::repeat_byte(4),
            "batch": { "calldata": "0x", "nonce": B256::ZERO, "deadline": 100 },
            "submission": { "signature": alloy::hex::encode_prefixed([0u8; 65]), "quote_id": null },
            "submission_status": "Accepted"
        }});
    }
    serde_json::from_value(serde_json::json!({
        "version": 2, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": "0x11111111111111111111111111111111", "index": 57,
        "address": Address::repeat_byte(3), "delegate": Address::repeat_byte(5),
        "retired": false, "issued": [],
        "swap_uses": [{ "id": "0x11111111111111111111111111111111", "started_at": 10,
            "role": { "PublicSourceDestination": { "origin_chain": 1,
                "source": Address::repeat_byte(6), "destination_token": Address::repeat_byte(7),
                "swap": swap
            }}
        }]
    }))
    .unwrap()
}

fn public_labels() -> PublicSwapLabels {
    PublicSwapLabels {
        source: "Main".into(),
        origin: "Ethereum".into(),
        destination: "Arbitrum One".into(),
        sell: "1,000 DAI".into(),
        sell_symbol: "DAI".into(),
        buy_symbol: "USDC".into(),
        bridged_symbol: "USDC".into(),
        traded: Some("1,000 DAI for 994.47 USDC".into()),
        deposited: Some("994.47 USDC".into()),
        received: Some("991.54 USDC".into()),
        held: Some("994.47 USDC".into()),
        held_named: true,
        minimum: "990.12 USDC".into(),
        account: "#57 · 0x0303…0303".into(),
        setup_block: Some(402_118_977),
    }
}

/// Every state of the order detail: its steps with their sub-lines and blocks, the facts
/// under them, and the footer's actions.
#[test]
fn public_swap_detail_shows_each_state_as_the_mockup_draws_it() {
    use super::super::public_progress::{
        PublicSwapAction as Action, PublicSwapFact as Fact, public_card_text, public_swap_actions,
        public_swap_facts, public_swap_note,
    };
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        PublicSwapDeposited, PublicSwapObservations, PublicSwapProxyHolding, SwapBridgeHandoff,
        SwapBridgeOutcome, SwapObservation,
    };
    type Case<'a> = (
        PublicSwapStage,
        ExecutorRecord,
        &'a PublicSwapLabels,
        u64,
        Vec<(&'a str, &'a str, Option<u64>)>,
        Vec<Fact>,
        Vec<Action>,
    );
    const VALID_TO: u32 = 1_000;
    const ACCOUNT: (&str, &str, Option<u64>) =
        ("Account on Arbitrum One set up", "#57 · 0x0303…0303", None);
    const APPROVED: (&str, &str, Option<u64>) = ("DAI approved from Main", "", Some(30));
    const FILLED: (&str, &str, Option<u64>) =
        ("Order filled", "1,000 DAI for 994.47 USDC", Some(51));
    const DEPOSITED: (&str, &str, Option<u64>) =
        ("Bridge deposit", "994.47 USDC to Across", Some(51));
    const UNDER_WAY: [Fact; 3] = [
        Fact::PaidFrom { copy: true },
        Fact::Destination,
        Fact::Started,
    ];
    const REFUND: [Fact; 3] = [Fact::Provider, Fact::DepositId, Fact::DepositExpired];
    const PROXY: [Fact; 3] = [Fact::CowProxy, Fact::ControlledBy, Fact::Settlement];
    let at = |number| SwapObservation {
        block: BlockNumHash::new(number, B256::ZERO),
        transaction_hash: Some(B256::ZERO),
    };
    let transaction = |kind: &str, block: u8| {
        serde_json::json!({
            "kind": kind, "transaction": {}, "hash": B256::repeat_byte(block),
            "inclusion": {
                "observation": at(u64::from(block)), "finalized": true, "succeeded": true
            }
        })
    };
    // A new destination account, an approval from the Public account and an accepted order.
    let order = |observed: PublicSwapObservations| {
        let mut value = serde_json::to_value(public_presentation_record(true, &observed)).unwrap();
        let claimed = &mut value["swap_uses"][0];
        claimed["fresh"] = serde_json::json!(true);
        let swap = &mut claimed["role"]["PublicSourceDestination"]["swap"];
        swap["transactions"] = serde_json::json!([transaction("Approval", 30)]);
        let mut uid = [0u8; 56];
        uid[52..].copy_from_slice(&VALID_TO.to_be_bytes());
        swap["path"]["Order"]["uid"] = serde_json::json!(alloy::hex::encode_prefixed(uid));
        serde_json::from_value::<ExecutorRecord>(value).unwrap()
    };
    // A reused destination account, an approval and the deposit the account sent itself.
    let deposit = |observed: PublicSwapObservations| {
        let mut value = serde_json::to_value(public_presentation_record(false, &observed)).unwrap();
        let swap = &mut value["swap_uses"][0]["role"]["PublicSourceDestination"]["swap"];
        swap["path"] = serde_json::json!("Deposit");
        swap["transactions"] =
            serde_json::json!([transaction("Approval", 30), transaction("Deposit", 34)]);
        serde_json::from_value::<ExecutorRecord>(value).unwrap()
    };
    let handed_off = |block, outcome| PublicSwapObservations {
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: at(block),
            deposit_id: Some(U256::from(1_279_830)),
        }),
        deposited: Some(PublicSwapDeposited {
            input_amount: U256::from(995),
            output_amount: U256::from(990),
        }),
        bridge_outcome: outcome,
        ..Default::default()
    };
    let bridged = |outcome| PublicSwapObservations {
        traded: Some(at(51)),
        ..handed_off(51, outcome)
    };
    let held = PublicSwapObservations {
        traded: Some(at(51)),
        held_by_proxy: Some(PublicSwapProxyHolding {
            observation: at(51),
            amount: U256::from(995),
        }),
        ..Default::default()
    };
    let open = PublicSwapLabels {
        traded: None,
        deposited: None,
        received: None,
        held: None,
        ..public_labels()
    };
    let direct = PublicSwapLabels {
        sell: "1,000 USDC".into(),
        sell_symbol: "USDC".into(),
        deposited: Some("1,000 USDC".into()),
        account: "#57 · 0x0303…0303 · reused".into(),
        setup_block: None,
        ..public_labels()
    };
    let full = public_labels();
    let valid_to = super::super::local_time_label(u64::from(VALID_TO));
    let expires = format!("expires {valid_to}");
    let expired = format!("expired {valid_to}");
    let cancelled = PublicSwapObservations {
        cancelled: Some(SwapObservation {
            transaction_hash: None,
            ..at(40)
        }),
        ..Default::default()
    };
    let cases: Vec<Case<'_>> = vec![
        (
            PublicSwapStage::OrderOpen,
            order(PublicSwapObservations::default()),
            &open,
            101,
            vec![
                ACCOUNT,
                APPROVED,
                ("Order open", expires.as_str(), None),
                ("Bridge deposit", "", None),
                ("Private on Arbitrum One", "", None),
            ],
            UNDER_WAY.to_vec(),
            vec![Action::CancelOrder],
        ),
        (
            PublicSwapStage::Bridging,
            deposit(handed_off(34, None)),
            &direct,
            101,
            vec![
                (
                    "Account on Arbitrum One ready",
                    "#57 · 0x0303…0303 · reused",
                    None,
                ),
                ("USDC approved from Main", "", Some(30)),
                ("Bridge deposit", "1,000 USDC to Across", Some(34)),
                ("Private on Arbitrum One", "Waiting for a relayer…", None),
            ],
            vec![
                Fact::PaidFrom { copy: true },
                Fact::Destination,
                Fact::Deposit,
            ],
            vec![Action::CheckStatus],
        ),
        (
            PublicSwapStage::DeliveredVerified,
            order(bridged(Some(SwapBridgeOutcome::DeliveredVerified {
                block: BlockNumHash::new(644, B256::ZERO),
                transaction_hash: B256::ZERO,
                output_amount: U256::from(995),
                shielded: true,
            }))),
            &full,
            101,
            vec![
                ACCOUNT,
                APPROVED,
                FILLED,
                DEPOSITED,
                (
                    "Private on Arbitrum One · verified",
                    "991.54 USDC shielded to your private balance",
                    Some(644),
                ),
            ],
            vec![
                Fact::Received,
                Fact::PaidFrom { copy: false },
                Fact::Destination,
                Fact::Provider,
                Fact::Fill,
                Fact::Settlement,
            ],
            vec![],
        ),
        (
            PublicSwapStage::Refunding,
            order(bridged(Some(SwapBridgeOutcome::Refunding))),
            &full,
            101,
            vec![
                ACCOUNT,
                APPROVED,
                FILLED,
                DEPOSITED,
                (
                    "Refunding to Main",
                    "No relayer delivered the deposit before it expired. Across returns 994.47 USDC to Main on Ethereum, usually within a few hours.",
                    None,
                ),
            ],
            REFUND.to_vec(),
            vec![Action::CheckStatus],
        ),
        (
            PublicSwapStage::Refunded,
            order(PublicSwapObservations {
                bridge_refund: Some(at(77)),
                ..bridged(Some(SwapBridgeOutcome::Refunding))
            }),
            &full,
            101,
            vec![
                ACCOUNT,
                APPROVED,
                FILLED,
                DEPOSITED,
                ("Refunded to Main", "", Some(77)),
            ],
            REFUND.to_vec(),
            vec![],
        ),
        (
            PublicSwapStage::HeldByProxy,
            order(held.clone()),
            &full,
            101,
            vec![
                ACCOUNT,
                APPROVED,
                FILLED,
                (
                    "Held by your CoW proxy",
                    "The swap went through, but the bridge deposit didn't run. 994.47 USDC is in Main's CoW proxy on Ethereum.",
                    Some(51),
                ),
            ],
            PROXY.to_vec(),
            vec![Action::CheckStatus, Action::Withdraw],
        ),
        (
            PublicSwapStage::SwappedNotBridged,
            order(PublicSwapObservations {
                withdrawn: Some(at(60)),
                deposit_ruled_out: Some(at(61)),
                ..held
            }),
            &full,
            101,
            vec![
                ACCOUNT,
                APPROVED,
                FILLED,
                ("Withdrawn to Main", "Swapped without bridging", Some(60)),
            ],
            PROXY.to_vec(),
            vec![],
        ),
        // The cancelled order's validity window is still open, so no retry yet.
        (
            PublicSwapStage::Cancelled,
            order(cancelled),
            &open,
            101,
            vec![
                ACCOUNT,
                APPROVED,
                ("Order cancelled", "", None),
                ("Bridge deposit", "", None),
                ("Private on Arbitrum One", "Nothing traded", None),
            ],
            UNDER_WAY.to_vec(),
            vec![Action::CheckStatus],
        ),
        (
            PublicSwapStage::Expired,
            order(PublicSwapObservations {
                expired: Some(at(90)),
                ..Default::default()
            }),
            &open,
            1_001,
            vec![
                ACCOUNT,
                APPROVED,
                ("Order expired", expired.as_str(), None),
                ("Bridge deposit", "", None),
                ("Private on Arbitrum One", "Nothing traded", None),
            ],
            UNDER_WAY.to_vec(),
            vec![Action::CheckStatus, Action::Retry],
        ),
        (
            PublicSwapStage::HeldOnDestination,
            order(bridged(Some(SwapBridgeOutcome::HeldOnDestination {
                block: BlockNumHash::new(644, B256::ZERO),
                transaction_hash: B256::ZERO,
                amount: U256::from(995),
            }))),
            &full,
            101,
            vec![
                ACCOUNT,
                APPROVED,
                FILLED,
                DEPOSITED,
                (
                    "Held on Arbitrum One",
                    "994.47 USDC is in the destination stealth account. Recover it on this network.",
                    Some(644),
                ),
            ],
            vec![
                Fact::PaidFrom { copy: false },
                Fact::Destination,
                Fact::Provider,
                Fact::Fill,
                Fact::Settlement,
            ],
            vec![Action::CheckStatus, Action::RecoverDestination],
        ),
    ];
    for (expected, record, labels, now, steps, facts, actions) in cases {
        let claimed = &record.swap_uses()[0];
        let swap = claimed.public_swap().unwrap();
        let stage = public_swap_stage(&record, claimed, None, now, None, None).unwrap();
        assert_eq!(stage, expected);
        let shown = public_swap_steps(claimed, stage, labels);
        assert_eq!(
            shown
                .iter()
                .map(|step| (step.label.as_str(), step.detail.as_str(), step.block))
                .collect::<Vec<_>>(),
            steps,
            "{stage:?}"
        );
        // The approval shows its own transaction, and the deposit step the observed
        // hand-off's.
        if stage == PublicSwapStage::Bridging {
            assert_eq!(
                shown
                    .iter()
                    .map(|step| step.transaction)
                    .collect::<Vec<_>>(),
                [None, Some(B256::repeat_byte(30)), Some(B256::ZERO), None]
            );
        }
        // The order step, and no other, carries the order ID.
        assert_eq!(
            shown
                .iter()
                .filter_map(|step| step.order)
                .collect::<Vec<_>>(),
            swap.order()
                .map(wallet_ops::vault::PublicSwapOrder::uid)
                .into_iter()
                .collect::<Vec<_>>(),
            "{stage:?}"
        );
        assert_eq!(public_swap_facts(stage), facts, "{stage:?}");
        let offered = public_swap_actions(swap, claimed, stage, now);
        assert_eq!(offered, actions, "{stage:?}");
        // Only an open order says it shields when it fills, and only it can be cancelled.
        let is_open = stage == PublicSwapStage::OrderOpen;
        let (_, subtitle) = public_card_text(stage, swap.intent().order, labels);
        assert_eq!(subtitle.ends_with("when it fills"), is_open, "{subtitle}");
        let note = public_swap_note(swap, stage, &offered, labels, now).unwrap_or_default();
        assert_eq!(note.contains("Cancelling now"), is_open, "{note}");
        assert_eq!(
            note.contains("You can retry after"),
            stage == PublicSwapStage::Cancelled,
            "{note}"
        );
    }
    // A reused account waits on no setup before signing: neither its stage nor its step
    // says it is being prepared.
    let reused = public_presentation_record(false, &PublicSwapObservations::default());
    let claimed = &reused.swap_uses()[0];
    let stage = public_swap_stage(&reused, claimed, None, 101, None, None).unwrap();
    assert_eq!(stage, PublicSwapStage::Preparing(SwapStage::Ready));
    assert_eq!(stage.label(), "Account ready");
    assert_eq!(
        public_swap_steps(claimed, stage, &direct)[0].label,
        "Account on Arbitrum One ready"
    );
    // A new account's step claims a setup only once it confirmed.
    let fresh = order(PublicSwapObservations::default());
    for (setup, title) in [
        (
            SwapStage::SetupPending,
            "Setting up account on Arbitrum One",
        ),
        (
            SwapStage::SetupFailed,
            "Account setup on Arbitrum One failed",
        ),
    ] {
        let steps = public_swap_steps(
            &fresh.swap_uses()[0],
            PublicSwapStage::Preparing(setup),
            &open,
        );
        assert_eq!((steps[0].label.as_str(), steps[0].block), (title, None));
    }
}

#[test]
fn public_swap_steps_distinguish_delivery_refund_proxy_and_direct_deposit() {
    use super::super::public_progress::{PublicSwapAction, public_swap_actions};
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        PublicSwapObservations, SwapBridgeHandoff, SwapBridgeOutcome, SwapObservation,
    };
    let observation = SwapObservation {
        block: BlockNumHash::new(20, B256::ZERO),
        transaction_hash: Some(B256::ZERO),
    };
    let outcome = |outcome| PublicSwapObservations {
        traded: Some(observation),
        bridge_handoff: Some(SwapBridgeHandoff {
            observation,
            deposit_id: Some(U256::from(1)),
        }),
        bridge_outcome: Some(outcome),
        ..Default::default()
    };
    for (observed, expected, status) in [
        (
            outcome(SwapBridgeOutcome::DeliveredVerified {
                block: observation.block,
                transaction_hash: B256::ZERO,
                output_amount: U256::from(995),
                shielded: true,
            }),
            PublicSwapStage::DeliveredVerified,
            PublicActionStepStatus::Done,
        ),
        (
            outcome(SwapBridgeOutcome::DeliveredReported {
                amount_out: None,
                transaction_hash: None,
                destination_transaction: None,
            }),
            PublicSwapStage::DeliveredReported,
            PublicActionStepStatus::Warning,
        ),
        (
            outcome(SwapBridgeOutcome::Refunding),
            PublicSwapStage::Refunding,
            PublicActionStepStatus::Warning,
        ),
        (
            PublicSwapObservations {
                bridge_refund: Some(observation),
                ..outcome(SwapBridgeOutcome::Refunding)
            },
            PublicSwapStage::Refunded,
            PublicActionStepStatus::Done,
        ),
        (
            outcome(SwapBridgeOutcome::HeldOnDestination {
                block: observation.block,
                transaction_hash: B256::ZERO,
                amount: U256::from(995),
            }),
            PublicSwapStage::HeldOnDestination,
            PublicActionStepStatus::Warning,
        ),
        (
            PublicSwapObservations {
                cancelled: Some(SwapObservation {
                    transaction_hash: None,
                    ..observation
                }),
                ..Default::default()
            },
            PublicSwapStage::Cancelled,
            PublicActionStepStatus::Stopped,
        ),
        (
            PublicSwapObservations {
                expired: Some(observation),
                ..Default::default()
            },
            PublicSwapStage::Expired,
            PublicActionStepStatus::Stopped,
        ),
    ] {
        let record = public_presentation_record(true, &observed);
        let claimed = &record.swap_uses()[0];
        let stage = public_swap_stage(&record, claimed, None, 101, None, None).unwrap();
        assert_eq!(stage, expected);
        let actions = super::super::public_progress::public_swap_actions(
            claimed.public_swap().unwrap(),
            claimed,
            stage,
            101,
        );
        assert_eq!(
            actions.contains(&super::super::public_progress::PublicSwapAction::RecoverDestination),
            stage == PublicSwapStage::HeldOnDestination
        );
        assert!(!actions.contains(&super::super::public_progress::PublicSwapAction::Withdraw));
        let steps = public_swap_steps(claimed, stage, &public_labels());
        assert_eq!(steps.last().unwrap().status, status);
        assert!(steps.iter().all(|step| step.children.is_empty()));
        if observed.bridge_handoff.is_some() {
            assert!(
                steps
                    .iter()
                    .any(|step| step.detail.starts_with("994.47 USDC to Across"))
            );
        }
    }
    let direct = public_presentation_record(false, &PublicSwapObservations::default());
    let steps = public_swap_steps(
        &direct.swap_uses()[0],
        PublicSwapStage::Depositing,
        &public_labels(),
    );
    assert_eq!(steps.len(), 3, "no order or approval without a transaction");
    assert_eq!(steps[1].status, PublicActionStepStatus::Pending);
    // A provisional revert can be reorganized, and every earlier attempt must resolve.
    for (inclusions, expected) in [
        (vec![(false, false)], PublicSwapStage::Depositing),
        (vec![(true, false)], PublicSwapStage::DepositFailed),
        (
            vec![(true, false), (false, false)],
            PublicSwapStage::Depositing,
        ),
        (
            vec![(true, false), (true, true)],
            PublicSwapStage::Depositing,
        ),
    ] {
        let mut value = serde_json::to_value(&direct).unwrap();
        value["swap_uses"][0]["role"]["PublicSourceDestination"]["swap"]["path"] =
            serde_json::json!("Deposit");
        value["swap_uses"][0]["role"]["PublicSourceDestination"]["swap"]["transactions"] = serde_json::json!(inclusions.into_iter().enumerate().map(|(index, (finalized, succeeded))| serde_json::json!({
            "kind": "Deposit", "transaction": {}, "hash": B256::repeat_byte(u8::try_from(index).unwrap()),
            "inclusion": { "observation": observation, "finalized": finalized, "succeeded": succeeded }
        })).collect::<Vec<_>>());
        let record: ExecutorRecord = serde_json::from_value(value).unwrap();
        assert_eq!(
            public_swap_stage(&record, &record.swap_uses()[0], None, 101, None, None),
            Some(expected)
        );
    }
    // A deposit ruled out by its used nonce returns the swap to preparation, where it can
    // continue or be cancelled.
    let ruled_out = SwapObservation {
        transaction_hash: None,
        ..observation
    };
    let mut value = serde_json::to_value(&direct).unwrap();
    let swap = &mut value["swap_uses"][0]["role"]["PublicSourceDestination"]["swap"];
    swap["path"] = serde_json::json!("Deposit");
    swap["transactions"] = serde_json::json!([{
        "kind": "Deposit", "transaction": {}, "hash": B256::ZERO, "ruled_out": ruled_out
    }]);
    let record: ExecutorRecord = serde_json::from_value(value).unwrap();
    let claimed = &record.swap_uses()[0];
    let stage = public_swap_stage(&record, claimed, None, 101, None, None).unwrap();
    assert!(matches!(stage, PublicSwapStage::Preparing(_)));
    let actions = public_swap_actions(claimed.public_swap().unwrap(), claimed, stage, 101);
    assert!(actions.contains(&PublicSwapAction::Continue));
    assert!(actions.contains(&PublicSwapAction::CancelPreparation));
    // Cancellation ends the order immediately, but its validity and hook deadlines still
    // gate a new review. Each deadline is inclusive.
    for (valid_to, batch_deadline) in [(100_u32, 200_u32), (200, 100)] {
        let record = public_presentation_record(
            true,
            &PublicSwapObservations {
                cancelled: Some(SwapObservation {
                    transaction_hash: None,
                    ..observation
                }),
                ..Default::default()
            },
        );
        let mut value = serde_json::to_value(record).unwrap();
        let order =
            &mut value["swap_uses"][0]["role"]["PublicSourceDestination"]["swap"]["path"]["Order"];
        let mut uid = [0u8; 56];
        uid[52..].copy_from_slice(&valid_to.to_be_bytes());
        order["uid"] = serde_json::json!(alloy::hex::encode_prefixed(uid));
        order["batch"]["deadline"] = serde_json::json!(batch_deadline);
        let record: ExecutorRecord = serde_json::from_value(value).unwrap();
        let claimed = &record.swap_uses()[0];
        let swap = claimed.public_swap().unwrap();
        assert!(
            !public_swap_actions(swap, claimed, PublicSwapStage::Cancelled, 200)
                .contains(&PublicSwapAction::Retry)
        );
        assert!(
            public_swap_actions(swap, claimed, PublicSwapStage::Cancelled, 201)
                .contains(&PublicSwapAction::Retry)
        );
    }
}

#[test]
fn public_withdrawal_waits_for_the_deposit_query_and_late_handoff_wins() {
    use alloy::eips::BlockNumHash;
    use wallet_ops::vault::{
        PublicSwapObservations, PublicSwapProxyHolding, SwapBridgeHandoff, SwapObservation,
    };
    let observation = SwapObservation {
        block: BlockNumHash::new(20, B256::ZERO),
        transaction_hash: None,
    };
    let held = PublicSwapObservations {
        traded: Some(observation),
        held_by_proxy: Some(PublicSwapProxyHolding {
            observation,
            amount: U256::from(995),
        }),
        ..Default::default()
    };
    for (observed, expected, group) in [
        (
            held.clone(),
            PublicSwapStage::HeldByProxy,
            SwapOrderGroup::NeedsRecovery,
        ),
        (
            PublicSwapObservations {
                withdrawn: Some(observation),
                ..held.clone()
            },
            PublicSwapStage::WithdrawnPendingCheck,
            SwapOrderGroup::Open,
        ),
        (
            PublicSwapObservations {
                withdrawn: Some(observation),
                deposit_ruled_out: Some(observation),
                ..held.clone()
            },
            PublicSwapStage::SwappedNotBridged,
            SwapOrderGroup::Ended,
        ),
        (
            PublicSwapObservations {
                withdrawn: Some(observation),
                bridge_handoff: Some(SwapBridgeHandoff {
                    observation,
                    deposit_id: Some(U256::from(1)),
                }),
                ..held
            },
            PublicSwapStage::Bridging,
            SwapOrderGroup::Open,
        ),
    ] {
        let record = public_presentation_record(true, &observed);
        let claimed = &record.swap_uses()[0];
        let stage = public_swap_stage(&record, claimed, None, 101, None, None).unwrap();
        assert_eq!(stage, expected);
        assert_eq!(stage.group(), group);
        let actions = super::super::public_progress::public_swap_actions(
            claimed.public_swap().unwrap(),
            claimed,
            stage,
            101,
        );
        assert_eq!(
            actions.contains(&super::super::public_progress::PublicSwapAction::Withdraw),
            stage == PublicSwapStage::HeldByProxy
        );
        assert!(
            !actions.contains(&super::super::public_progress::PublicSwapAction::RecoverDestination)
        );
        let steps = public_swap_steps(claimed, stage, &public_labels());
        assert_eq!(
            steps.len(),
            if stage == PublicSwapStage::Bridging {
                4
            } else {
                3
            }
        );
        assert_eq!(
            steps.last().unwrap().status,
            if stage == PublicSwapStage::HeldByProxy {
                PublicActionStepStatus::Warning
            } else if stage == PublicSwapStage::SwappedNotBridged {
                PublicActionStepStatus::Done
            } else {
                PublicActionStepStatus::Pending
            }
        );
    }
}
