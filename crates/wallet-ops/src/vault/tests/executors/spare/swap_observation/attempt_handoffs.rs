use super::*;

#[tokio::test]
async fn a_retrys_settlement_hands_off_with_the_expired_attempts_across_deposit() {
    // A retry signs its hooks at the expired attempt's nonces, so the expired attempt's
    // post-hook can still run in the retry's settlement and deposit under its own terms. The
    // trade is the retry's; the hand-off and the surplus belong to the expired attempt.
    // 0: the expired attempt's deposit and surplus shield. 1: both attempts signed the same
    // terms, so the traded order keeps the hand-off. 2: two expired attempts signed the
    // deposit's terms, and the earliest takes the hand-off. 3: one deposit under each
    // attempt's terms.
    for scenario in 0..4 {
        let fixture = Fixture::start().await;
        let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
        let (delivery, older) = across_order(spoke_pool);
        let newer = if scenario == 1 {
            older
        } else {
            AcrossOrderTerms {
                output_amount: U256::from(9_900),
                quote_timestamp: older.quote_timestamp + 600,
                fill_deadline: older.fill_deadline + 600,
                ..older
            }
        };
        expired_across_attempt(&fixture, 1, 20, delivery, older).await;
        // The retry's attempt number and `validTo` block, and its settlement's block.
        let (attempt, valid_to, block) = if scenario == 2 {
            expired_across_attempt(&fixture, 2, 26, delivery, older).await;
            (3, 60, 40)
        } else {
            (2, 40, 30)
        };
        let (retry_uid, retry_index) = (uid(attempt, valid_to), usize::from(attempt) - 1);
        fixture.record_delivery_attempt(
            attempt,
            valid_to,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(newer)),
        );
        {
            let record = fixture.record();
            let orders = record.swap().unwrap().orders();
            assert_eq!(
                orders[0].pre_hook().nonce(),
                orders[retry_index].pre_hook().nonce(),
                "the expired attempt's post-hook stays executable"
            );
        }

        // The retry's trade, then the expired attempt's deposit and surplus shield.
        let deposit = signed_deposit(delivery, &older);
        let mut logs = retry_settlement_logs(&fixture, retry_uid, &older, deposit);
        if scenario == 3 {
            let executor_payment = Transfer {
                from: EXECUTOR,
                to: older.spoke_pool,
                value: older.input_amount,
            }
            .encode_log_data();
            logs.extend([
                (BUY, executor_payment),
                (
                    newer.spoke_pool,
                    signed_deposit(delivery, &newer).encode_log_data(),
                ),
            ]);
        }
        let observed = settle_retry(&fixture, retry_uid, block, logs).await;
        if scenario == 3 {
            // Conflicting deposits stay unresolved, and the whole read is refused.
            assert!(observed.is_err(), "scenario {scenario}");
            let record = fixture.record();
            assert!(
                record.swap().unwrap().orders().iter().all(|order| {
                    let observed = order.observations();
                    observed.traded.is_none()
                        && observed.bridge_handoff.is_none()
                        && observed.delivered.is_none()
                }),
                "scenario {scenario}"
            );
            fixture.finish().await;
            continue;
        }
        let traded = observed.unwrap();
        let handoff = SwapBridgeHandoff {
            observation: traded,
            deposit_id: Some(U256::from(42)),
        };

        let record = fixture.record();
        let orders = record.swap().unwrap().orders();
        let (expired, retry) = (orders[0].observations(), orders[retry_index].observations());
        assert_eq!(retry.traded, Some(traded), "the trade is the retry's");
        assert!(
            retry.trade_amounts.is_some(),
            "the retry keeps its trade's amounts"
        );
        assert_eq!(
            (expired.traded, expired.trade_amounts),
            (None, None),
            "no trade is invented for the expired attempt"
        );
        match scenario {
            0 => {
                assert_eq!(
                    (
                        retry.bridge_handoff,
                        retry.delivered,
                        retry.settlement_credit
                    ),
                    (None, None, None),
                    "the retry's terms did not make this deposit"
                );
                assert_eq!(
                    (expired.bridge_handoff, expired.delivered),
                    (Some(handoff), Some(traded)),
                    "the hand-off is the expired attempt's, whose payload made the deposit"
                );
                assert_eq!(
                    expired
                        .settlement_credit
                        .map(|credit| credit.private_amount),
                    Some(U256::from(74)),
                    "surplus follows the matched payload's shield request"
                );
                assert_eq!(
                    record
                        .swap_bridges_to_track()
                        .map(SwapOrderRecord::uid)
                        .collect::<Vec<_>>(),
                    vec![uid(1, 20)],
                    "destination tracking follows the actual deposit"
                );
                let swap = record.swap().unwrap();
                assert_eq!(
                    (
                        swap.trade_order(&orders[0]).map(SwapOrderRecord::uid),
                        swap.delivery_order(&orders[retry_index])
                            .map(SwapOrderRecord::uid)
                    ),
                    (Some(uid(2, 40)), Some(uid(1, 20)))
                );
                // Complete source evidence stops rereads of the settlement block.
                fixture.chain.lock().unwrap().rpc_requests.clear();
                fixture
                    .owner
                    .observe_swap_settlement(
                        fixture.operation,
                        uid(2, 40),
                        30,
                        crate::OperationHttpClient::direct_for_tests(),
                    )
                    .await
                    .unwrap();
                assert_eq!(fixture.chain.lock().unwrap().rpc_requests, [] as [Value; 0]);
                assert_eq!(fixture.record(), record);
                // Check status once the nonce passed both hooks. The executor holds the buy
                // amount again, which alone would read as an undelivered trade.
                {
                    let mut chain = fixture.chain.lock().unwrap();
                    chain.nonces.push((30, 3));
                    chain.nullifiers_spent = Some(30);
                    chain.buy_balance.push((30, BUY_AMOUNT));
                    chain.rpc_requests.clear();
                }
                let record = fixture.observe(33, 31).await;
                let orders = record.swap().unwrap().orders();
                let (expired, retry) = (orders[0].observations(), orders[1].observations());
                assert_eq!(
                    (expired.post_hook_deposit, retry.post_hook_deposit),
                    (Some(traded), None),
                    "only the expired attempt signed these terms, so the deposit names its hook"
                );
                assert_eq!(
                    (retry.delivered, retry.undelivered),
                    (None, None),
                    "the use's delivery is the expired attempt's hand-off"
                );
                assert!(!reads_a_balance(
                    &fixture.chain.lock().unwrap().rpc_requests
                ));
            }
            1 => {
                // The shield in the receipt is the expired attempt's request, so the traded
                // order, which owns the terms, takes no credit from it.
                assert_eq!(
                    state(&record, 1),
                    SwapOrderState::Bridging,
                    "scenario {scenario}"
                );
                assert_eq!(
                    (
                        retry.bridge_handoff,
                        retry.delivered,
                        retry.settlement_credit
                    ),
                    (Some(handoff), Some(traded), None),
                    "scenario {scenario}"
                );
                assert_eq!(
                    (
                        expired.bridge_handoff,
                        expired.delivered,
                        expired.settlement_credit
                    ),
                    (None, None, None),
                    "scenario {scenario}"
                );
                // Check status once the nonce passed both hooks. Either attempt's post-hook
                // could have made the deposit, so neither is named and no page is read to
                // find out.
                {
                    let mut chain = fixture.chain.lock().unwrap();
                    chain.nonces.push((30, 3));
                    chain.nullifiers_spent = Some(30);
                }
                let log_queries = || fixture.chain.lock().unwrap().log_queries;
                let before = log_queries();
                let record = fixture.observe(33, 31).await;
                for order in record.swap().unwrap().orders() {
                    let observed = order.observations();
                    assert_eq!(
                        (
                            observed.post_hook_deposit,
                            observed.shielded,
                            observed.settlement_credit
                        ),
                        (None, None, None),
                        "the hook that ran is not named and no surplus is invented"
                    );
                }
                assert_eq!(
                    (state(&record, 0), state(&record, 1)),
                    (
                        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired),
                        SwapOrderState::Bridging
                    )
                );
                assert_eq!(
                    (
                        record.swap_hook_winner(U256::from(2)),
                        record.resolved_swap_hook(U256::from(2))
                    ),
                    (None, None)
                );
                assert_eq!(
                    log_queries(),
                    before,
                    "the source outcome is complete and both hook nonces are consumed"
                );
                let record = fixture.observe(34, 32).await;
                assert_eq!(log_queries(), before, "the search is not repeated");
                let observations = |record: &ExecutorRecord| {
                    record
                        .swap()
                        .unwrap()
                        .orders()
                        .iter()
                        .map(SwapOrderRecord::observations)
                        .collect::<Vec<_>>()
                };
                let restarted = restarted_owner(&fixture);
                let reopened = restarted
                    .observe_swap(
                        fixture.operation,
                        32..34,
                        crate::OperationHttpClient::direct_for_tests(),
                    )
                    .await
                    .unwrap()
                    .record()
                    .clone();
                assert_eq!(log_queries(), before, "nor after a restart");
                assert_eq!(observations(&reopened), observations(&record));
                // The open bridge outcome keeps the account from another swap.
                let reuse = || {
                    restarted.reuse_swap_account(
                        fixture.operation,
                        33,
                        SwapAccountRole::Source,
                        SwapAccountUse::New,
                        crate::OperationHttpClient::direct_for_tests(),
                    )
                };
                let refused = reuse().await.unwrap_err();
                assert_eq!(
                    refused.downcast_ref::<SwapAccountRefusal>(),
                    Some(&SwapAccountRefusal::BridgeUndelivered),
                    "{refused:#}"
                );
                fixture
                    .store
                    .record_swap_bridge_outcome(
                        fixture.operation,
                        uid(2, 40),
                        SwapBridgeOutcome::DeliveredVerified {
                            block: BlockNumHash::new(500, B256::repeat_byte(0x60)),
                            transaction_hash: B256::repeat_byte(0x61),
                            output_amount: older.output_amount,
                            shielded: false,
                        },
                    )
                    .unwrap();
                let reused = reuse().await.unwrap();
                assert_eq!(reused.expected_pre_hook_nonce(), U256::from(3));
                for order in fixture.record().swap().unwrap().orders() {
                    let observed = order.observations();
                    assert_eq!(
                        (observed.post_hook_deposit, observed.shielded),
                        (None, None),
                        "admission names no hook either"
                    );
                }
                restarted.shutdown().await;
            }
            // Scenario 2; the conflicting deposits of scenario 3 returned above.
            _ => {
                assert_eq!(
                    (expired.bridge_handoff, expired.delivered),
                    (Some(handoff), Some(traded)),
                    "scenario {scenario}"
                );
                for later in [orders[1].observations(), retry] {
                    assert_eq!(
                        (later.bridge_handoff, later.delivered),
                        (None, None),
                        "scenario {scenario}"
                    );
                }
            }
        }
        fixture.finish().await;
    }
}

// A retry traded while the expired attempt's Across post-hook made the deposit. The outcome on
// the destination chain and the refund follow that deposit: both are checked against the
// expired attempt's signed terms and recorded on it, and both attempts read as the bridge's
// state. The account takes another swap only after the delivery.
#[tokio::test]
async fn a_split_swaps_bridge_outcome_follows_the_expired_attempts_deposit() {
    // The expired attempt's state as an attempt of its use, then the retry's.
    let states = |record: &ExecutorRecord| {
        let orders = record.swap().unwrap().orders();
        (
            swap_attempt_state(record, &orders[0]),
            swap_attempt_state(record, &orders[1]),
        )
    };
    let outcomes = |record: &ExecutorRecord| {
        let orders = record.swap().unwrap().orders();
        (
            orders[0].observations().bridge_outcome,
            orders[1].observations().bridge_outcome,
        )
    };
    for (surplus, refunds) in [
        (BridgeSurplus::Reshield, false),
        (BridgeSurplus::KeepInAccount, true),
    ] {
        let variant = format!("{surplus:?}, refunds: {refunds}");
        let fixture = Fixture::start().await;
        let (delivery, older) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
        let delivery = BridgeDelivery {
            surplus,
            ..delivery
        };
        let newer = AcrossOrderTerms {
            output_amount: U256::from(9_900),
            quote_timestamp: older.quote_timestamp + 600,
            fill_deadline: older.fill_deadline + 600,
            ..older
        };
        expired_across_attempt(&fixture, 1, 20, delivery, older).await;
        fixture.record_delivery_attempt(
            2,
            40,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(newer)),
        );
        let mut logs = retry_settlement_logs(
            &fixture,
            uid(2, 40),
            &older,
            signed_deposit(delivery, &older),
        );
        let keeps = surplus == BridgeSurplus::KeepInAccount;
        if keeps {
            // The post-hook keeps the surplus rather than shielding it.
            logs.truncate(logs.len() - 3);
        }
        let traded = settle_retry(&fixture, uid(2, 40), 30, logs).await.unwrap();

        let record = fixture.record();
        let orders = record.swap().unwrap().orders();
        let (expired, retry) = (orders[0].observations(), orders[1].observations());
        assert_eq!(
            (expired.bridge_handoff, expired.delivered, retry.traded),
            (
                Some(SwapBridgeHandoff {
                    observation: traded,
                    deposit_id: Some(U256::from(42)),
                }),
                Some(traded),
                Some(traded)
            ),
            "{variant}"
        );
        assert_eq!(
            (
                expired
                    .settlement_credit
                    .map(|credit| credit.private_amount),
                retry.settlement_credit
            ),
            ((!keeps).then_some(U256::from(74)), None),
            "{variant}"
        );
        assert_eq!(
            states(&record),
            (SwapOrderState::Bridging, SwapOrderState::Bridging),
            "{variant}"
        );
        assert_eq!(state(&record, 1), SwapOrderState::Traded, "{variant}");
        let read_before_trade = record.nonce_observation();

        // Check status once the nonce passed both hooks.
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.nonces.push((30, 3));
            chain.nullifiers_spent = Some(30);
        }
        let record = fixture.observe(33, 31).await;
        assert_eq!(
            states(&record),
            (SwapOrderState::Bridging, SwapOrderState::Bridging),
            "{variant}"
        );
        let (destination, config, server) = across_destination(42161).await;
        let reloaded = || {
            ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1)
                .unwrap()
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == fixture.operation)
                .unwrap()
        };

        if refunds {
            // Across refunds the expired attempt's deposit to the stealth account.
            let refund = Transfer {
                from: older.spoke_pool,
                to: EXECUTOR,
                value: older.input_amount,
            };
            let refund_tx = {
                let mut chain = fixture.chain.lock().unwrap();
                chain.add_addressed_transaction(
                    34,
                    older.spoke_pool,
                    Bytes::new(),
                    vec![(older.input_token, refund.encode_log_data())],
                );
                chain.head = 36;
                chain.transactions.last().unwrap().0
            };
            let deposit = format!(
                r#"{{"deposit":{{"status":"expired","depositRefundTxHash":"{refund_tx}","outputAmount":"1","recipient":"{}","destinationChainId":"42161"}}}}"#,
                delivery.receiver,
            );
            let (url, _, stub) =
                crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(move |_| {
                    deposit.clone()
                })
                .await;
            let clients = bridge_clients(&fixture, &url);
            assert_eq!(
                observe_bridge(&fixture, &clients, &config).await,
                Some(SwapBridgeOutcome::Refunding),
                "{variant}"
            );
            let record = fixture.record();
            assert_eq!(
                outcomes(&record),
                (Some(SwapBridgeOutcome::Refunding), None),
                "{variant}"
            );
            assert_eq!(
                states(&record),
                (SwapOrderState::Refunding, SwapOrderState::Refunding),
                "{variant}"
            );
            assert_eq!(
                reuse_refusal(&fixture.owner, fixture.operation, 35).await,
                SwapAccountRefusal::BridgeRefunding,
                "{variant}"
            );

            // After a restart, an explicit check verifies the refund against the expired
            // attempt's deposit and records it there.
            let restarted = restarted_owner(&fixture);
            assert_eq!(
                restarted
                    .check_swap_bridge(
                        fixture.operation,
                        uid(1, 20),
                        &clients,
                        Some(&config),
                        crate::OperationHttpClient::direct_for_tests()
                    )
                    .await
                    .unwrap(),
                Some(SwapBridgeOutcome::Refunding),
                "{variant}"
            );
            let verified = SwapObservation {
                block: fixture.chain.lock().unwrap().block(34),
                transaction_hash: Some(refund_tx),
            };
            let record = reloaded();
            let orders = record.swap().unwrap().orders();
            assert_eq!(
                (
                    orders[0].observations().bridge_refund,
                    orders[1].observations().bridge_refund
                ),
                (Some(verified), None),
                "{variant}"
            );
            assert_eq!(
                states(&record),
                (SwapOrderState::Refunding, SwapOrderState::Refunding),
                "{variant}"
            );
            assert_eq!(
                reuse_refusal(&restarted, fixture.operation, 35).await,
                SwapAccountRefusal::BridgeRefunding,
                "{variant}"
            );
            restarted.shutdown().await;
            stub.abort();
        } else {
            fixture.chain.lock().unwrap().head = 34;
            assert_eq!(
                reuse_refusal(&fixture.owner, fixture.operation, 33).await,
                SwapAccountRefusal::BridgeUndelivered,
                "{variant}"
            );
            // A relayer fills deposit 42 under the expired attempt's terms, which sign an
            // output of 9,800. The retry signed 9,900, and no fill pays that.
            let spoke_pool = config.bridge_profile().unwrap().spoke_pool();
            let expected = {
                let mut chain = destination.lock().unwrap();
                chain.add_addressed_transaction(
                    30,
                    spoke_pool,
                    Bytes::new(),
                    vec![(
                        spoke_pool,
                        filled_relay(delivery, &older, 42).encode_log_data(),
                    )],
                );
                chain.head = 31;
                let (transaction_hash, transaction, _) = &chain.transactions[0];
                SwapBridgeOutcome::DeliveredVerified {
                    block: BlockNumHash::new(30, transaction.block_hash.unwrap()),
                    transaction_hash: *transaction_hash,
                    output_amount: U256::from(9_800),
                    shielded: false,
                }
            };
            let (url, lookups, stub) =
                crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(|_| {
                    across_report("filled", REPORTED_AMOUNT)
                })
                .await;
            let clients = bridge_clients(&fixture, &url);
            assert_eq!(
                observe_bridge(&fixture, &clients, &config).await,
                Some(expected.clone()),
                "{variant}"
            );
            assert!(
                lookups
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(path, _)| path == "/api/deposit?originChainId=1&depositId=42")
            );
            let record = fixture.record();
            assert_eq!(
                outcomes(&record),
                (Some(expected.clone()), None),
                "{variant}"
            );
            assert_eq!(
                states(&record),
                (SwapOrderState::Done, SwapOrderState::Done),
                "{variant}"
            );
            // Both attempts are settled history once the account was read past the trade.
            assert!(record.settled_swaps_at(33), "{variant}");
            // The pair settles at the trade's block and no lower. The account read from
            // before the trade stands in here, so the nonce does not set the cutoff.
            let mut early = serde_json::to_value(&record).unwrap();
            early["nonce_observation"] = serde_json::to_value(read_before_trade).unwrap();
            let early: ExecutorRecord = serde_json::from_value(early).unwrap();
            assert!(
                !early.settled_swaps_at(29) && early.settled_swaps_at(30),
                "{variant}"
            );

            // A restart keeps the outcome, and the account takes another swap at the nonce
            // past both hooks.
            let restarted = restarted_owner(&fixture);
            let record = reloaded();
            assert_eq!(outcomes(&record), (Some(expected), None), "{variant}");
            assert_eq!(
                states(&record),
                (SwapOrderState::Done, SwapOrderState::Done),
                "{variant}"
            );
            let reused = restarted
                .reuse_swap_account(
                    fixture.operation,
                    33,
                    SwapAccountRole::Source,
                    SwapAccountUse::New,
                    crate::OperationHttpClient::direct_for_tests(),
                )
                .await
                .unwrap();
            assert_eq!(reused.expected_pre_hook_nonce(), U256::from(3), "{variant}");
            restarted.shutdown().await;
            stub.abort();
        }
        server.abort();
        fixture.finish().await;
    }
}

/// A relayer fills Across deposit 42 under `terms` on Arbitrum in a finalized block, and the
/// hand-off held by `order` is tracked. Returns the tracked outcome and the verified delivery
/// of `terms` it should be.
async fn track_across_fill(
    fixture: &Fixture,
    order: OrderUid,
    delivery: BridgeDelivery,
    terms: &AcrossOrderTerms,
) -> (Option<SwapBridgeOutcome>, SwapBridgeOutcome) {
    let (destination, config, server) = across_destination(42161).await;
    let spoke_pool = config.bridge_profile().unwrap().spoke_pool();
    let expected = {
        let mut chain = destination.lock().unwrap();
        chain.add_addressed_transaction(
            30,
            spoke_pool,
            Bytes::new(),
            vec![(
                spoke_pool,
                filled_relay(delivery, terms, 42).encode_log_data(),
            )],
        );
        chain.head = 31;
        across_verified(&chain, terms, 0)
    };
    let (url, _, stub) =
        crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(|_| {
            across_report("filled", REPORTED_AMOUNT)
        })
        .await;
    let clients = bridge_clients(fixture, &url);
    let tracked = fixture
        .owner
        .observe_swap_bridge(
            fixture.operation,
            order,
            &clients,
            Some(&config),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    stub.abort();
    server.abort();
    (tracked, expected)
}

// Two live Across attempts sign their post-hooks at one nonce, so either deposit can follow
// either trade in the settlement. The earlier order trades and the receipt holds the deposit
// the speed-up signed: the trade stays on the earlier order, and the hand-off and the outcome
// on the destination chain follow the speed-up's terms, also after a restart.
#[tokio::test]
async fn a_live_orders_trade_hands_off_with_the_speed_ups_across_deposit() {
    let fixture = Fixture::start().await;
    let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
    // Attempt numbers only label the fixture's orders. The speed-up is attempt 1, whose
    // post-hook's surplus shield `retry_settlement_logs` puts in the receipt.
    let (delivery, replacement) = across_order(spoke_pool);
    let earlier = AcrossOrderTerms {
        output_amount: U256::from(9_900),
        ..replacement
    };
    let (traded_uid, speed_up_uid) = (uid(2, 100), uid(1, 110));
    fixture.record_delivery_attempt(
        2,
        100,
        BUY,
        SwapDelivery::Bridge(delivery),
        Some(BridgeOrderTerms::Across(earlier)),
    );
    record_bridge_speed_up(
        &fixture,
        1,
        110,
        traded_uid,
        BridgeOrderTerms::Across(replacement),
    );
    let logs = retry_settlement_logs(
        &fixture,
        traded_uid,
        &replacement,
        signed_deposit(delivery, &replacement),
    );
    let traded = settle_retry(&fixture, traded_uid, 30, logs).await.unwrap();

    let record = fixture.record();
    let swap = record.swap().unwrap();
    let [first, speed_up] = swap.orders() else {
        panic!("the speed-up is the second order");
    };
    let (trade, handed) = (first.observations(), speed_up.observations());
    assert_eq!(
        (trade.traded, trade.bridge_handoff, trade.delivered),
        (Some(traded), None, None)
    );
    assert!(trade.trade_amounts.is_some() && trade.settlement_credit.is_none());
    assert_eq!(
        (handed.traded, handed.bridge_handoff, handed.delivered),
        (
            None,
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: Some(U256::from(42)),
            }),
            Some(traded)
        )
    );
    assert_eq!(
        handed.settlement_credit.map(|credit| credit.private_amount),
        Some(U256::from(74)),
        "surplus follows the shield request the speed-up signed"
    );
    assert_eq!(
        (
            swap.delivery_order(first).map(SwapOrderRecord::uid),
            swap.trade_order(speed_up).map(SwapOrderRecord::uid)
        ),
        (Some(speed_up_uid), Some(traded_uid))
    );
    assert_eq!(
        record
            .swap_bridges_to_track()
            .map(SwapOrderRecord::uid)
            .collect::<Vec<_>>(),
        vec![speed_up_uid]
    );

    // A relayer fills the deposit under the speed-up's terms.
    let (tracked, expected) =
        track_across_fill(&fixture, speed_up_uid, delivery, &replacement).await;
    assert_eq!(tracked, Some(expected.clone()));
    let record = stored(&fixture);
    let orders = record.swap().unwrap().orders();
    assert_eq!(
        (
            orders[0].observations().bridge_outcome,
            orders[1].observations().bridge_outcome
        ),
        (None, Some(expected))
    );
    for order in orders {
        assert_eq!(swap_attempt_state(&record, order), SwapOrderState::Done);
    }
    fixture.finish().await;
}

// An attempt approved for a destination minimum of 9,000 expired unused, a retry approved for
// 10,000 is open, and a speed-up approved for 9,500 trades. The receipt's deposit is the one
// the first attempt's post-hook signed, so the hand-off and the verified delivery of 9,000 are
// that attempt's. The trade, its amounts and the surplus credit keep their own evidence, and
// nothing records the speed-up's 9,500 as delivered.
#[tokio::test]
async fn a_speed_ups_trade_delivers_under_the_expired_attempts_lower_across_terms() {
    let fixture = Fixture::start().await;
    let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
    let signed = |output: u64, later: u32| AcrossOrderTerms {
        output_amount: U256::from(output),
        quote_timestamp: terms.quote_timestamp + later,
        fill_deadline: terms.fill_deadline + later,
        ..terms
    };
    let (original, higher, lower) = (signed(9_000, 0), signed(10_000, 600), signed(9_500, 1_200));
    let (original_uid, retry_uid, speed_up_uid) = (uid(1, 20), uid(2, 60), uid(3, 70));
    expired_across_attempt(&fixture, 1, 20, delivery, original).await;
    fixture.record_delivery_attempt(
        2,
        60,
        BUY,
        SwapDelivery::Bridge(delivery),
        Some(BridgeOrderTerms::Across(higher)),
    );
    record_bridge_speed_up(&fixture, 3, 70, retry_uid, BridgeOrderTerms::Across(lower));
    let logs = retry_settlement_logs(
        &fixture,
        speed_up_uid,
        &original,
        signed_deposit(delivery, &original),
    );
    let traded = settle_retry(&fixture, speed_up_uid, 30, logs)
        .await
        .unwrap();

    let record = fixture.record();
    let swap = record.swap().unwrap();
    let [expired, retry, speed_up] = swap.orders() else {
        panic!("three attempts of one use");
    };
    // The speed-up relaxed the open retry's fill limit, whatever its destination minimum.
    assert!(speed_up.bounds().buy_amount < retry.bounds().buy_amount);
    let (delivering, trading) = (expired.observations(), speed_up.observations());
    assert_eq!(
        (trading.traded, trading.bridge_handoff, trading.delivered),
        (Some(traded), None, None)
    );
    assert_eq!(
        trading
            .trade_amounts
            .map(|amounts| (amounts.sell_amount, amounts.buy_amount)),
        Some((U256::from(SELL_AMOUNT), U256::from(BUY_AMOUNT)))
    );
    assert_eq!(trading.settlement_credit, None);
    assert_eq!(
        (
            delivering.traded,
            delivering.trade_amounts,
            delivering.bridge_handoff,
            delivering.delivered
        ),
        (
            None,
            None,
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: Some(U256::from(42)),
            }),
            Some(traded)
        )
    );
    assert_eq!(
        delivering
            .settlement_credit
            .map(|credit| credit.private_amount),
        Some(U256::from(74))
    );
    assert_eq!(retry.observations(), SwapOrderObservations::default());
    assert_eq!(
        (
            swap.delivery_order(speed_up).map(SwapOrderRecord::uid),
            swap.trade_order(expired).map(SwapOrderRecord::uid)
        ),
        (Some(original_uid), Some(speed_up_uid))
    );

    // A relayer fills the deposit under the first attempt's terms, which sign 9,000.
    let (tracked, expected) = track_across_fill(&fixture, original_uid, delivery, &original).await;
    assert_eq!(tracked, Some(expected));
    let record = stored(&fixture);
    let [expired, retry, speed_up] = record.swap().unwrap().orders() else {
        panic!("three attempts of one use");
    };
    let Some(SwapBridgeOutcome::DeliveredVerified { output_amount, .. }) =
        expired.observations().bridge_outcome
    else {
        panic!("the delivery is verified on the attempt that holds the hand-off");
    };
    assert_eq!(
        (output_amount, Some(output_amount)),
        (original.output_amount, expired.bounds().destination_minimum)
    );
    for later in [retry, speed_up] {
        assert_eq!(later.observations().bridge_outcome, None);
        assert!(Some(output_amount) < later.bounds().destination_minimum);
    }
    for order in [expired, speed_up] {
        assert_eq!(swap_attempt_state(&record, order), SwapOrderState::Done);
    }
    fixture.finish().await;
}

#[tokio::test]
async fn settlement_receipts_hand_off_to_near_intents_with_the_trade() {
    let fixture = Fixture::start().await;
    let traded = near_handoff(&fixture, Address::repeat_byte(0x79)).await;
    let record = fixture.record();
    let observed = record.swap().unwrap().orders()[0].observations();
    assert_eq!(state(&record, 0), SwapOrderState::Bridging);
    assert_eq!(
        (
            observed.bridge_handoff,
            observed.delivered,
            observed.settlement_credit
        ),
        (
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: None,
            }),
            Some(traded),
            None
        )
    );
    fixture.finish().await;
}
