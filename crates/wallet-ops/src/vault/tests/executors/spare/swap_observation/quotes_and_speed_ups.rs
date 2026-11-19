use super::*;

#[tokio::test]
async fn recorded_swap_quote_defers_nonce_and_reorg_checks_until_preparation() {
    let fixture = Fixture::start().await;
    let restarted = restarted_owner(&fixture);
    let requests = fixture.chain.lock().unwrap().rpc_methods.len();
    // Even after restart, quoting the recorded setup needs no chain observation.
    let executor = restarted
        .swap_order_preview(fixture.operation, false, None)
        .unwrap();
    assert_eq!(fixture.chain.lock().unwrap().rpc_methods.len(), requests);
    assert!(!executor.requires_setup(), "do not pay for setup again");
    assert!(
        executor.delegated().is_none(),
        "a quote cannot authorize signing"
    );
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let profile = chains.get(1).unwrap().swap_profile().unwrap();
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: fixture.railgun,
    };
    let amount = U256::from(1_000_000);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, SELL, amount, [7; 16]),
        0,
        4,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let crate::SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
        &builder,
        &profile,
        executor,
        std::slice::from_ref(&input),
        &crate::SwapAmountRequest {
            sell_token: SELL,
            buy_token: BUY,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("one note fits the quote");
    };
    let mut review = crate::price_swap_review(
        plan,
        serde_json::from_value(json!({
            "quote": {
                "sellToken": SELL, "buyToken": BUY,
                "sellAmount": "996500", "buyAmount": "3000000000",
                "validTo": 1, "feeAmount": "1000", "gasAmount": "0", "gasPrice": "0",
                "sellTokenPrice": "1000000000000", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }))
        .unwrap(),
        crate::SwapPrice::Unverified,
        U256::from(25),
        U256::from(25),
        50,
        crate::cow::GAS_SHARE_BALANCED_BPS,
        std::time::Duration::from_mins(10),
        1,
        U256::ZERO,
        crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
    )
    .unwrap();
    let minimum = review.suggested_private_minimum();
    let approval = review.approval(minimum, true).unwrap();
    let quoted_request = review.plan().proof_request(&profile).unwrap();
    let quoted_inputs = builder
        .preview_mixed_private_action_plan(std::slice::from_ref(&input), &quoted_request)
        .unwrap()
        .selected_inputs;
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.nonces.push((15, 2));
    }
    assert_eq!(
        fixture
            .owner
            .refresh_swap_executor(
                &mut review,
                15,
                SwapAccountUse::Claimed(crate::vault::SwapUseId::first(fixture.operation)),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        review
            .plan()
            .swap_executor()
            .delegated()
            .unwrap()
            .observed()
            .nonce(),
        U256::from(2)
    );
    assert_eq!(review.approval(minimum, true).unwrap(), approval);
    // Use the same request and rebuild validator as proving: the fresh nonce must not
    // invalidate the reviewed shape or permit replacing its pinned notes.
    let request = review.plan().proof_request(&profile).unwrap();
    let rebuilt = builder
        .preview_mixed_private_action_plan(std::slice::from_ref(&input), &request)
        .expect("the refreshed nonce must support rebuilding the reviewed pre-hook");
    assert_eq!(rebuilt.selected_inputs, quoted_inputs);
    let mut replacement = input.clone();
    replacement.position += 1;
    assert!(matches!(
        builder.preview_mixed_private_action_plan(&[replacement], &request),
        Err(railgun_wallet::tx::BuildError::PinnedInputUnavailable { .. })
    ));
    let mut changed_nonce = request;
    let Some(railgun_wallet::tx::CompositeAdapter::Executor(context)) = &mut changed_nonce.adapter
    else {
        panic!("the pre-hook request names its executor");
    };
    context.execution_nonce += U256::ONE;
    assert!(matches!(
        builder.preview_mixed_private_action_plan(&[input], &changed_nonce),
        Err(railgun_wallet::tx::BuildError::CompositePlanShapeChanged { .. })
    ));
    // Losing the setup to a reorg must stop execution even though the local quote succeeded.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.reorg(11);
        chain.nonces.clear();
    }
    assert!(
        fixture
            .owner
            .refresh_swap_executor(
                &mut review,
                15,
                SwapAccountUse::Claimed(crate::vault::SwapUseId::first(fixture.operation)),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .is_err()
    );
    restarted.shutdown().await;
    fixture.finish().await;
}

#[tokio::test]
async fn reusing_an_account_for_another_pair_does_not_credit_the_old_tokens_shield() {
    let fixture = Fixture::start_for_purpose("Private send").await;
    assert!(!crate::is_swap_record(&fixture.record()));
    // The list reads recorded outcomes, so a set-up account is offered before a restarted
    // session reconciles it.
    let listed_after_restart = listed(&restarted_owner(&fixture), fixture.operation).unwrap();
    assert_eq!(listed_after_restart.address(), EXECUTOR);
    let selected = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            13,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(selected.executor(), EXECUTOR);
    assert!(selected.is_reused());
    fixture.record_attempt(0, 28);
    assert!(
        listed(&fixture.owner, fixture.operation).is_none(),
        "an order that can still execute keeps the account out of the list"
    );
    // The same order keeps it from either role, with the reason the form shows.
    for role in [
        SwapAccountRole::Source,
        SwapAccountRole::Destination { token: OTHER_BUY },
    ] {
        assert_eq!(
            fixture
                .owner
                .swap_account_refusal(fixture.operation, role)
                .unwrap(),
            Some(SwapAccountRefusal::PreviousOrderLive)
        );
    }
    let expired = fixture.observe(31, 20).await;
    assert!(expired.swap().unwrap().admits_attempt());
    let visible = listed(&fixture.owner, fixture.operation)
        .expect("an account whose order ended is offered for another swap");
    assert_eq!(visible.last_pair(), Some((SELL, BUY)));
    // Hiding it keeps it out of the list; admission below still takes it.
    fixture.store.set_hidden(fixture.operation, true).unwrap();
    assert!(listed(&fixture.owner, fixture.operation).is_none());
    fixture.chain.lock().unwrap().rpc_methods.clear();
    let preview = fixture
        .owner
        .swap_order_preview(fixture.operation, true, None)
        .unwrap();
    assert!(preview.is_reused());
    assert_eq!(fixture.chain.lock().unwrap().rpc_methods, [] as [String; 0]);
    // The ended order isn't settled history, so admission reconciles it before judging.
    let selected = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            30,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(selected.executor(), EXECUTOR);
    assert!(selected.is_reused());
    assert_eq!(
        fixture
            .chain
            .lock()
            .unwrap()
            .rpc_methods
            .iter()
            .filter(|method| method.as_str() == "eth_blockNumber")
            .count(),
        1,
        "checking setup and prior orders must share one account read"
    );
    // Another pair is another swap use of the account.
    fixture
        .store
        .claim_swap_pair(crate::vault::SwapPairClaim {
            id: crate::vault::SwapUseId::random().unwrap(),
            source: crate::vault::SwapAccountChoice::Existing(fixture.operation),
            delegate: fixture.delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval: SwapApproval {
                bounds: swap_bounds(),
                price_verified: None,
                price_acknowledged: false,
                delivery: SwapDelivery::Reshield,
                tokens: Some(SwapApprovalTokens {
                    sell: SELL,
                    buy: OTHER_BUY,
                }),
                accounts: None,
            },
            destination: None,
        })
        .unwrap();
    fixture.record_pair_attempt(1, 100, OTHER_BUY);
    let refused = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            30,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.downcast_ref::<SwapAccountRefusal>(),
        Some(&SwapAccountRefusal::PreviousOrderLive),
        "{refused:#}"
    );
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        let trade = Trade {
            owner: EXECUTOR,
            sellToken: SELL,
            buyToken: OTHER_BUY,
            sellAmount: U256::from(SELL_AMOUNT),
            buyAmount: U256::from(BUY_AMOUNT),
            feeAmount: U256::ZERO,
            orderUid: uid(1, 100).0.to_vec().into(),
        };
        logs.push((fixture.settlement, trade.encode_log_data()));
        // An older post-hook wins nonce 2 and shields USDC, not this order's DAI.
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_975)));
        chain.add_logs(33, B256::repeat_byte(0x49), logs);
        chain.nonces.push((33, 3));
    }
    let observed = fixture.observe(35, 31).await;
    assert_eq!(state(&observed, 1), SwapOrderState::Traded);
    assert!(
        observed.swap().unwrap().orders()[0]
            .observations()
            .shielded
            .is_some()
    );
    assert!(
        observed.swap().unwrap().orders()[1]
            .observations()
            .delivered
            .is_none()
    );
    assert!(!observed.swap().unwrap().admits_attempt());
    fixture.finish().await;
}

#[tokio::test]
async fn only_a_speed_up_is_admitted_while_the_first_order_is_still_live() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    // The pre-hook's nonce is unused, nothing is filled, and `validTo` is ahead.
    let open = fixture.observe(31, 20).await;
    assert_eq!(state(&open, 0), SwapOrderState::Open);
    let swap_use = SwapUseId::first(fixture.operation);
    assert_eq!(open.active_swap_use(), Some(swap_use));
    let first = uid(0, 100);
    let binding = SwapSpeedUp {
        swap_use,
        order: first,
        pre_hook_nonce: open.swap().unwrap().orders()[0].pre_hook().nonce(),
    };

    // The swap's own use is refused the account as a retry while that order can still execute.
    let refused = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            30,
            SwapAccountRole::Source,
            SwapAccountUse::Claimed(swap_use),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.downcast_ref::<SwapAccountRefusal>(),
        Some(&SwapAccountRefusal::PreviousOrderLive),
        "{refused:#}"
    );
    // A speed-up that names the live order is admitted at that order's nonce.
    let admitted = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            30,
            SwapAccountRole::Source,
            SwapAccountUse::SpeedUp(binding),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(admitted.expected_pre_hook_nonce(), binding.pre_hook_nonce);

    // The replacement shares the use, the pre-hook's nonce and the proof, sells the same
    // amount and asks for less.
    let live = fixture.record();
    let swap = live.swap().unwrap();
    let first_order = &swap.orders()[0];
    let speed_up = speed_up_attempt(&fixture, 1, 110, first);
    assert_eq!(speed_up.use_id, swap_use);
    assert_eq!(speed_up.pre_hook.nonce(), first_order.pre_hook().nonce());
    assert_eq!(&speed_up.proof, swap.proof());
    assert_eq!(
        speed_up.bounds.sell_amount,
        first_order.bounds().sell_amount
    );
    assert!(speed_up.bounds.buy_amount < first_order.bounds().buy_amount);
    // Recorded as an ordinary retry it is refused, since the live order isn't ended.
    assert!(matches!(
        fixture.store.record_swap_attempt(
            fixture.operation,
            SwapAttempt {
                speed_up: None,
                ..speed_up.clone()
            },
        ),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    assert_eq!(fixture.record(), live);
    // A speed-up is refused when it asks for the open order's buy amount, funds from another
    // proof, or doesn't invalidate the open order.
    for refused in [
        SwapAttempt {
            bounds: first_order.bounds().clone(),
            ..speed_up.clone()
        },
        SwapAttempt {
            proof: SwapProof::new(B256::repeat_byte(0x21), swap.proof().inputs().to_vec()),
            ..speed_up.clone()
        },
        SwapAttempt {
            invalidates: None,
            ..speed_up.clone()
        },
    ] {
        assert!(matches!(
            fixture
                .store
                .record_swap_attempt(fixture.operation, refused),
            Err(ExecutorStoreError::SwapSpeedUpRefused)
        ));
        assert_eq!(fixture.record(), live);
    }

    // The speed-up is recorded beside the live order: one use, one pre-hook nonce, one proof.
    fixture
        .store
        .record_swap_attempt(fixture.operation, speed_up.clone())
        .unwrap();
    let record = fixture.record();
    let [kept, added] = record.swap().unwrap().orders() else {
        panic!("the speed-up is the second order");
    };
    assert_eq!((kept.uid(), added.uid()), (first, speed_up.uid));
    assert_eq!(added.use_id(), kept.use_id());
    assert_eq!(added.pre_hook().nonce(), kept.pre_hook().nonce());
    assert_eq!(added.invalidates(), Some(first));
    assert_eq!(record.swap().unwrap().proof(), swap.proof());
    // The live order and its use keep what was approved for them, and the new order has its
    // own bounds.
    assert_eq!(kept, first_order);
    assert_eq!(record.swap_uses(), live.swap_uses());
    assert_eq!(added.bounds(), &speed_up.bounds);
    // Both orders survive reopening the store.
    let reopened = ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1)
        .unwrap()
        .records()
        .unwrap()
        .into_iter()
        .find(|reopened| reopened.operation() == fixture.operation)
        .unwrap();
    assert_eq!(reopened, record);

    // The earlier order is no longer the latest, so another speed-up can't name it.
    assert!(matches!(
        fixture
            .store
            .record_swap_attempt(fixture.operation, speed_up_attempt(&fixture, 2, 120, first)),
        Err(ExecutorStoreError::SwapSpeedUpRefused)
    ));
    assert_eq!(fixture.record(), record);
    fixture.finish().await;
}

// The account rule and the store admit a speed-up for an order paid from the private balance
// unless it delivers privately through a bridge, and only for the binding of the latest order
// while the account's nonce is still the one both pre-hooks share.
#[tokio::test]
async fn speed_up_admission_follows_the_delivery_the_binding_and_the_shared_nonce() {
    let receiver = Address::repeat_byte(0x77);
    for scenario in 0..5 {
        let fixture = Fixture::start().await;
        let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
        // 0: Private delivery. 1: a Public address. 2: Across to a public receiver. 3: NEAR
        // Intents to a public receiver. 4: Across to the wallet's private balance.
        let (delivery, bridge, supported) = match scenario {
            0 => (SwapDelivery::Reshield, None, true),
            1 => (SwapDelivery::External { receiver }, None, true),
            2 => {
                let (delivery, terms) = across_order(spoke_pool);
                (
                    SwapDelivery::Bridge(delivery),
                    Some(BridgeOrderTerms::Across(terms)),
                    true,
                )
            }
            3 => (
                SwapDelivery::Bridge(BridgeDelivery {
                    provider: BridgeProvider::NearIntents,
                    destination_chain: 56,
                    receiver,
                    destination_token: Address::ZERO,
                    surplus: BridgeSurplus::BridgedByProvider,
                    private: None,
                }),
                Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
                    deposit_address: Address::repeat_byte(0x79),
                    min_amount_out: U256::from(15),
                    amount_out: U256::from(16),
                    deadline: "2026-09-30T01:00:00.000Z".into(),
                    signed_quote: "{}".into(),
                })),
                true,
            ),
            _ => {
                let (delivery, terms) =
                    private_across_order(spoke_pool, Address::repeat_byte(0x7a));
                (
                    SwapDelivery::Bridge(delivery),
                    Some(BridgeOrderTerms::Across(terms)),
                    false,
                )
            }
        };
        fixture.record_delivery_attempt(1, 100, BUY, delivery.clone(), bridge);
        let record = fixture.record();
        let open = &record.swap().unwrap().orders()[0];
        let binding = |order: &SwapOrderRecord| SwapSpeedUp {
            swap_use: order.use_id().unwrap(),
            order: order.uid(),
            pre_hook_nonce: order.pre_hook().nonce(),
        };
        let refusal = |record: &ExecutorRecord, speed_up| {
            swap_account_refusal(
                record,
                1,
                SwapAccountRole::Source,
                SwapAccountUse::SpeedUp(speed_up),
                SwapAdmissionEvidence::Fresh,
            )
        };
        let unsupported = (!supported).then_some(SwapAccountRefusal::SpeedUpUnsupported);
        assert_eq!(
            refusal(&record, binding(open)),
            unsupported,
            "scenario {scenario}"
        );
        // A binding to another order, or to another nonce than the open order's pre-hook, is
        // not this swap's open order.
        for changed in [
            SwapSpeedUp {
                order: uid(9, 100),
                ..binding(open)
            },
            SwapSpeedUp {
                pre_hook_nonce: open.pre_hook().nonce() + U256::ONE,
                ..binding(open)
            },
        ] {
            assert_eq!(
                refusal(&record, changed),
                Some(SwapAccountRefusal::SpeedUpOrderChanged),
                "scenario {scenario}"
            );
        }

        let stored = fixture.store.record_swap_attempt(
            fixture.operation,
            speed_up_attempt(&fixture, 2, 110, open.uid()),
        );
        if !supported {
            assert!(
                matches!(stored, Err(ExecutorStoreError::SwapSpeedUpRefused)),
                "scenario {scenario}"
            );
            assert_eq!(fixture.record(), record);
            fixture.finish().await;
            continue;
        }
        let stored = stored.unwrap();
        let latest = stored.swap().unwrap().orders().last().unwrap();
        assert_eq!(latest.delivery(), delivery, "scenario {scenario}");
        assert_eq!(
            refusal(&stored, binding(latest)),
            None,
            "scenario {scenario}"
        );

        // The account's nonce moves past the shared one with no pre-hook attributed. Either
        // order's pre-hook may have taken it, so nothing is signed at it again.
        let observed = stored.nonce_observation().unwrap();
        let moved = fixture
            .store
            .record_account_read(
                fixture.operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(observed.block().number + 1, B256::repeat_byte(0xab)),
                    observed.nonce() + U256::ONE,
                ),
            )
            .unwrap();
        assert!(
            moved
                .swap()
                .unwrap()
                .orders()
                .iter()
                .all(|order| order.observations().pre_hook_executed.is_none())
        );
        assert_eq!(
            refusal(&moved, binding(latest)),
            Some(SwapAccountRefusal::SpeedUpNonceUsed),
            "scenario {scenario}"
        );
        fixture.finish().await;
    }
}

// A pre-hook invalidates one order, so a speed-up waits while two orders can still fill. An
// order without a recorded expiry counts until local time is past its `validTo` by the
// profile's whole validity window.
#[tokio::test]
async fn a_speed_up_waits_until_its_open_order_is_the_only_one_that_can_fill() {
    let fixture = Fixture::start().await;
    let profile = swap_profile();
    // The second order ends more than a validity window after the first.
    let (first, second) = (uid(0, 100), uid(1, 160));
    let (first_ends, second_ends) = (at_block(100), at_block(160));
    assert!(first_ends + profile.valid_to_window() < second_ends);
    let judged =
        |order, now| crate::speed_up_block(&fixture.record(), &profile, 1, order, now, None);
    let binding = |order| SwapSpeedUp {
        swap_use: SwapUseId::first(fixture.operation),
        order,
        pre_hook_nonce: U256::ONE,
    };

    // One open order can be sped up.
    fixture.record_attempt(0, 100);
    assert_eq!(judged(first, at_block(20)), Ok(binding(first)));
    fixture
        .store
        .record_swap_attempt(fixture.operation, speed_up_attempt(&fixture, 1, 160, first))
        .unwrap();
    assert_eq!(
        judged(second, at_block(20)),
        Err(SpeedUpBlock::TwoOrdersFillable)
    );

    // Local time at the first order's `validTo` plus the window still counts it. One second
    // later it drops out, and the second order is the open one.
    let boundary = first_ends + profile.valid_to_window();
    assert_eq!(
        judged(second, boundary),
        Err(SpeedUpBlock::TwoOrdersFillable)
    );
    assert_eq!(
        judged(second, boundary + Duration::from_secs(1)),
        Ok(binding(second))
    );
    // An order at its own `validTo` is not sped up.
    assert_eq!(judged(second, second_ends), Err(SpeedUpBlock::Expired));

    // A recorded finalized expiry of the first order admits the second at once, while both
    // are still unexpired by local time.
    let seen = SwapObservation {
        block: fixture.record().nonce_observation().unwrap().block(),
        transaction_hash: None,
    };
    for expiry in [
        SwapOrderObservations {
            pre_hook_dead: Some(SwapPreHookDeath {
                cause: SwapPreHookDeathCause::Expired,
                observation: seen,
            }),
            ..SwapOrderObservations::default()
        },
        SwapOrderObservations {
            expired: Some(seen),
            ..SwapOrderObservations::default()
        },
    ] {
        fixture
            .store
            .record_swap_observations(fixture.operation, first, expiry)
            .unwrap();
        assert_eq!(judged(second, at_block(20)), Ok(binding(second)));
    }
    fixture.finish().await;
}
