use super::*;

#[tokio::test]
async fn near_intents_reports_set_the_outcome_and_final_ones_stop_tracking() {
    let destination_tx = B256::repeat_byte(2);
    let bnb =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(56)
            .cloned()
            .unwrap();
    for (status, expected, expected_state) in [
        (
            "SUCCESS",
            Some(SwapBridgeOutcome::DeliveredReported {
                amount_out: Some(U256::from(16)),
                transaction_hash: Some(destination_tx),
                destination_transaction: None,
            }),
            SwapOrderState::Done,
        ),
        (
            "REFUNDED",
            Some(SwapBridgeOutcome::Refunding),
            SwapOrderState::Refunding,
        ),
        (
            "FAILED",
            Some(SwapBridgeOutcome::NeedsAttention),
            SwapOrderState::NeedsAttention,
        ),
        (
            "INCOMPLETE_DEPOSIT",
            Some(SwapBridgeOutcome::NeedsAttention),
            SwapOrderState::NeedsAttention,
        ),
        ("PROCESSING", None, SwapOrderState::Bridging),
    ] {
        let fixture = Fixture::start().await;
        near_handoff(&fixture, Address::repeat_byte(0x79)).await;
        let report = format!(
            r#"{{"status":"{status}","swapDetails":{{"amountOut":"16","destinationChainTxHashes":[{{"hash":"{destination_tx}","explorerUrl":"https://bscscan.com/tx/{destination_tx}"}}]}}}}"#
        );
        let (url, requests, stub) =
            crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(move |_| {
                report.clone()
            })
            .await;
        let clients = bridge_clients(&fixture, &url);
        assert_eq!(
            observe_bridge(&fixture, &clients, &bnb).await,
            expected,
            "{status}"
        );
        let record = fixture.record();
        assert_eq!(
            record.swap().unwrap().orders()[0]
                .observations()
                .bridge_outcome,
            expected,
            "{status}"
        );
        assert_eq!(state(&record, 0), expected_state, "{status}");
        // Only an order without an outcome is polled again.
        assert_eq!(
            record.swap_bridges_to_track().count(),
            usize::from(expected.is_none()),
            "{status}"
        );
        if status == "SUCCESS" {
            // 1Click's report is final. Only a delivery Across reported is open to a later
            // check.
            for replacement in [
                SwapBridgeOutcome::Refunding,
                SwapBridgeOutcome::DeliveredVerified {
                    block: BlockNumHash::new(30, B256::repeat_byte(30)),
                    transaction_hash: destination_tx,
                    output_amount: U256::from(16),
                    shielded: false,
                },
            ] {
                assert!(matches!(
                    fixture.store.record_swap_bridge_outcome(
                        fixture.operation,
                        uid(1, 20),
                        replacement
                    ),
                    Err(ExecutorStoreError::InvalidRecord)
                ));
            }
        }
        if expected == Some(SwapBridgeOutcome::NeedsAttention) {
            // Routine polling leaves it alone; an explicit status check asks again.
            assert_eq!(observe_bridge(&fixture, &clients, &bnb).await, expected);
            assert_eq!(requests.lock().unwrap().len(), 1);
            assert_eq!(
                fixture
                    .owner
                    .check_swap_bridge(
                        fixture.operation,
                        uid(1, 20),
                        &clients,
                        Some(&bnb),
                        crate::OperationHttpClient::direct_for_tests()
                    )
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(requests.lock().unwrap().len(), 2);
        }
        stub.abort();
        fixture.finish().await;
    }
}

// A non-EVM delivery is tracked as a NEAR Intents one, by its deposit address alone, and
// without a destination chain: 1Click's success keeps the destination transaction as 1Click
// reports it, a refund returns the deposit to the stealth account, and a failure needs
// attention. No RPC request is made for any of them.
#[tokio::test]
async fn non_evm_reports_set_the_outcome_without_reading_a_chain() {
    let signature =
        "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW";
    for (status, expected, expected_state) in [
        (
            "SUCCESS",
            Some(SwapBridgeOutcome::DeliveredReported {
                amount_out: Some(U256::from(16)),
                transaction_hash: None,
                destination_transaction: Some(signature.to_owned()),
            }),
            SwapOrderState::Done,
        ),
        (
            "REFUNDED",
            Some(SwapBridgeOutcome::Refunding),
            SwapOrderState::Refunding,
        ),
        (
            "FAILED",
            Some(SwapBridgeOutcome::NeedsAttention),
            SwapOrderState::NeedsAttention,
        ),
    ] {
        let fixture = Fixture::start().await;
        let deposit_address = Address::repeat_byte(0x79);
        record_near_delivery(
            &fixture,
            SwapDelivery::NonEvmBridge(crate::vault::NonEvmDelivery {
                network: crate::bridge::NonEvmNetwork::Solana,
                receiver: "So11111111111111111111111111111111111111112".to_owned(),
                asset_id: "nep141:sol.omft.near".to_owned(),
                contract: None,
                symbol: "SOL".to_owned(),
                decimals: 9,
            }),
            deposit_address,
        );
        settle_near_order(&fixture, deposit_address).await;
        let report = format!(
            r#"{{"status":"{status}","swapDetails":{{"amountOut":"16","destinationChainTxHashes":[{{"hash":"{signature}","explorerUrl":"https://solscan.io/tx/{signature}"}}]}}}}"#
        );
        let (url, requests, stub) =
            crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(move |_| {
                report.clone()
            })
            .await;
        let clients = bridge_clients(&fixture, &url);
        fixture.chain.lock().unwrap().rpc_requests.clear();
        let outcome = fixture
            .owner
            .observe_swap_bridge(
                fixture.operation,
                uid(1, 20),
                &clients,
                None,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(outcome, expected, "{status}");
        let record = fixture.record();
        assert_eq!(
            record.swap().unwrap().orders()[0]
                .observations()
                .bridge_outcome,
            expected,
            "{status}"
        );
        assert_eq!(state(&record, 0), expected_state, "{status}");
        assert_eq!(requests.lock().unwrap().len(), 1, "{status}");
        assert_eq!(
            fixture.chain.lock().unwrap().rpc_requests,
            [] as [Value; 0],
            "{status}"
        );
        stub.abort();
        fixture.finish().await;
    }
}

#[tokio::test]
async fn a_providers_report_before_the_recorded_handoff_is_returned_and_records_nothing() {
    use crate::SwapBridgeReport::{Delivered, Failed, Pending, Refunded};
    let delivered = |amount: u64, transaction: Option<B256>| Delivered {
        amount_out: Some(U256::from(amount)),
        seen: None,
        transaction,
    };
    // Across is asked about the hinted deposit, and not at all without one. NEAR Intents is
    // asked by the order's deposit address.
    for (across, status, hinted, expected) in [
        (
            true,
            "filled",
            true,
            delivered(REPORTED_AMOUNT, Some(REPORTED_FILL)),
        ),
        (true, "pending", true, Pending),
        (true, "expired", true, Refunded),
        (true, "filled", false, Pending),
        (false, "SUCCESS", false, delivered(16, None)),
        (false, "PROCESSING", false, Pending),
        (false, "FAILED", false, Failed),
    ] {
        let fixture = Fixture::start().await;
        let report = if across {
            let (delivery, terms) =
                across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
            fixture.record_delivery_attempt(
                1,
                20,
                BUY,
                SwapDelivery::Bridge(delivery),
                Some(BridgeOrderTerms::Across(terms)),
            );
            across_report(status, REPORTED_AMOUNT)
        } else {
            record_near_order(&fixture, Address::repeat_byte(0x79));
            format!(r#"{{"status":"{status}","swapDetails":{{"amountOut":"16"}}}}"#)
        };
        let handoff = hinted.then_some(crate::SwapHandoffHint {
            block: BlockNumHash::new(15, B256::repeat_byte(15)),
            deposit_id: Some(U256::from(42)),
            sent: U256::from(BUY_AMOUNT - 99),
        });
        let (url, requests, stub) =
            crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(move |_| {
                report.clone()
            })
            .await;
        let clients = bridge_clients(&fixture, &url);
        let before = fixture.record();
        assert_eq!(
            fixture
                .owner
                .swap_bridge_report(
                    fixture.operation,
                    uid(1, 20),
                    handoff,
                    &clients,
                    None,
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            expected,
            "{status}"
        );
        assert_eq!(fixture.record(), before, "{status}");
        assert_eq!(
            requests.lock().unwrap().len(),
            usize::from(!across || hinted),
            "{status}"
        );
        stub.abort();
        fixture.finish().await;
    }
}

// The fill block Across names is read on the destination chain before it is final there, and
// the report says what the verification would later find in the same receipt.
#[tokio::test]
async fn a_private_across_report_says_what_the_unconfirmed_fill_block_shows_and_records_nothing() {
    use crate::SwapDeliverySeen::{Held, Shielded};
    // Whether the fill's receipt holds the shield, and whether the endpoint serves whole-block
    // receipts.
    for (shield, served) in [(true, true), (false, true), (true, false)] {
        let fixture = Fixture::start().await;
        // Block 30 is the head, above finality depth 1.
        let (destination, config, server) = across_destination(42161).await;
        let profile = config.bridge_profile().unwrap();
        let (spoke_pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
        let railgun = config.require_railgun().unwrap().deployment.contract;
        let (delivery, terms) = private_across_order(
            fixture.config.bridge_profile().unwrap().spoke_pool(),
            handler,
        );
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let received = terms.output_amount + U256::from(7);
        let transfer = |from: Address, to: Address, value: U256| {
            (
                terms.output_token,
                Transfer { from, to, value }.encode_log_data(),
            )
        };
        let mut logs = vec![
            (
                spoke_pool,
                filled_relay(delivery, &terms, 42).encode_log_data(),
            ),
            transfer(handler, delivery.receiver, received),
        ];
        if shield {
            logs.push(transfer(
                delivery.receiver,
                railgun,
                received - U256::from(25),
            ));
        }
        {
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(30, spoke_pool, Bytes::new(), logs);
            if !served {
                chain.receipt_error = Some(-32601);
            }
        }
        let report = across_report("filled", REPORTED_AMOUNT);
        let (url, _, stub) =
            crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(move |_| {
                report.clone()
            })
            .await;
        let clients = bridge_clients(&fixture, &url);
        let handoff = Some(crate::SwapHandoffHint {
            block: BlockNumHash::new(15, B256::repeat_byte(15)),
            deposit_id: Some(U256::from(42)),
            sent: U256::from(BUY_AMOUNT - 99),
        });
        let before = fixture.record();
        let seen = match (served, shield) {
            (false, _) => None,
            (true, true) => Some(Shielded { amount: received }),
            (true, false) => Some(Held { amount: received }),
        };
        assert_eq!(
            fixture
                .owner
                .swap_bridge_report(
                    fixture.operation,
                    uid(1, 20),
                    handoff,
                    &clients,
                    Some(&config),
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            crate::SwapBridgeReport::Delivered {
                amount_out: Some(U256::from(REPORTED_AMOUNT)),
                seen,
                transaction: Some(REPORTED_FILL),
            },
            "shield {shield}, served {served}"
        );
        assert_eq!(fixture.record(), before);
        stub.abort();
        server.abort();
        fixture.finish().await;
    }
}

// Each NEAR Intents attempt signs its own deposit address. The trade is the expired attempt's,
// so tracking and the outcome follow its address and signed quote, not the newer attempt's.
#[tokio::test]
async fn near_intents_tracking_follows_the_traded_attempts_deposit_address() {
    let destination_tx = B256::repeat_byte(2);
    let bnb =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(56)
            .cloned()
            .unwrap();
    let fixture = Fixture::start().await;
    let delivery = SwapDelivery::Bridge(BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 56,
        receiver: Address::repeat_byte(0x77),
        destination_token: Address::ZERO,
        surplus: BridgeSurplus::BridgedByProvider,
        private: None,
    });
    let (older_address, newer_address) = (Address::repeat_byte(0x79), Address::repeat_byte(0x69));
    let older = BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
        deposit_address: older_address,
        min_amount_out: U256::from(15),
        amount_out: U256::from(16),
        deadline: "2026-09-30T01:00:00.000Z".into(),
        signed_quote: r#"{"quote":"older"}"#.into(),
    });
    let newer = BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
        deposit_address: newer_address,
        min_amount_out: U256::from(17),
        amount_out: U256::from(18),
        deadline: "2026-09-30T02:00:00.000Z".into(),
        signed_quote: r#"{"quote":"newer"}"#.into(),
    });
    let (traded_uid, newer_uid) = (uid(1, 20), uid(2, 40));

    // The first attempt expires unfilled, and the retry signs another deposit address.
    fixture.record_delivery_attempt(1, 20, BUY, delivery.clone(), Some(older.clone()));
    let record = fixture.observe(23, 21).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    fixture.record_delivery_attempt(2, 40, BUY, delivery, Some(newer));

    // A finalized settlement then holds the first attempt's trade, which pays its address.
    let logs = vec![
        (fixture.settlement, trade_log(traded_uid)),
        (
            BUY,
            Transfer {
                from: fixture.settlement,
                to: older_address,
                value: U256::from(BUY_AMOUNT),
            }
            .encode_log_data(),
        ),
    ];
    let traded = settle_retry(&fixture, traded_uid, 30, logs).await.unwrap();

    // The uids of the attempt that traded for the first attempt's delivery, and of the
    // attempt that delivered its trade.
    let pairing = |record: &ExecutorRecord| {
        let swap = record.swap().unwrap();
        let first = &swap.orders()[0];
        (
            swap.trade_order(first).map(SwapOrderRecord::uid),
            swap.delivery_order(first).map(SwapOrderRecord::uid),
        )
    };
    let outcomes = |record: &ExecutorRecord| {
        let orders = record.swap().unwrap().orders();
        (
            orders[0].observations().bridge_outcome,
            orders[1].observations().bridge_outcome,
        )
    };
    let record = fixture.record();
    let orders = record.swap().unwrap().orders();
    let (first, retry) = (orders[0].observations(), orders[1].observations());
    assert_eq!(
        (first.traded, first.bridge_handoff, first.delivered),
        (
            Some(traded),
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: None,
            }),
            Some(traded)
        )
    );
    assert_eq!(
        (retry.traded, retry.bridge_handoff, retry.delivered),
        (None, None, None)
    );
    assert_eq!(pairing(&record), (Some(traded_uid), Some(traded_uid)));
    let tracked = record.swap_bridges_to_track().collect::<Vec<_>>();
    assert_eq!(
        tracked.iter().map(|order| order.uid()).collect::<Vec<_>>(),
        vec![traded_uid]
    );
    // The tracked order carries the first attempt's address and signed quote.
    assert_eq!(tracked[0].bridge(), Some(&older));

    // 1Click reports the first attempt's deposit delivered and the retry's address refunded.
    let (asked_older, asked_newer) = (older_address.to_string(), newer_address.to_string());
    let (answers_older, answers_newer) = (asked_older.clone(), asked_newer.clone());
    let (url, requests, stub) = crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub_with(move |path, _| {
        if path.contains(&answers_older) {
            (
                200,
                format!(
                    r#"{{"status":"SUCCESS","swapDetails":{{"amountOut":"16","destinationChainTxHashes":[{{"hash":"{destination_tx}","explorerUrl":"https://bscscan.com/tx/{destination_tx}"}}]}}}}"#
                ),
            )
        } else if path.contains(&answers_newer) {
            (200, r#"{"status":"REFUNDED"}"#.to_owned())
        } else {
            (400, "{}".to_owned())
        }
    })
    .await;
    let clients = bridge_clients(&fixture, &url);
    let expected = Some(SwapBridgeOutcome::DeliveredReported {
        amount_out: Some(U256::from(16)),
        transaction_hash: Some(destination_tx),
        destination_transaction: None,
    });
    assert_eq!(observe_bridge(&fixture, &clients, &bnb).await, expected);
    assert_eq!(outcomes(&fixture.record()), (expected.clone(), None));
    // Only the first attempt's address was asked about.
    let only_older_asked = || {
        let requests = requests.lock().unwrap();
        requests.len() == 1
            && requests
                .iter()
                .all(|(path, _)| path.contains(&asked_older) && !path.contains(&asked_newer))
    };
    assert!(only_older_asked());

    // The retry holds no hand-off, so its status is neither recorded nor asked for.
    assert!(matches!(
        fixture.store.record_swap_bridge_outcome(
            fixture.operation,
            newer_uid,
            SwapBridgeOutcome::Refunding
        ),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    assert_eq!(
        fixture
            .owner
            .observe_swap_bridge(
                fixture.operation,
                newer_uid,
                &clients,
                Some(&bnb),
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        None
    );
    assert!(only_older_asked());

    // A restart keeps the pairing and the outcome, and asks 1Click nothing more.
    let restarted = restarted_owner(&fixture);
    let record = ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1)
        .unwrap()
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == fixture.operation)
        .unwrap();
    assert_eq!(pairing(&record), (Some(traded_uid), Some(traded_uid)));
    assert_eq!(outcomes(&record), (expected.clone(), None));
    assert_eq!(record.swap_bridges_to_track().count(), 0);
    assert_eq!(
        swap_attempt_state(&record, &record.swap().unwrap().orders()[0]),
        SwapOrderState::Done
    );
    for attempt in [traded_uid, newer_uid] {
        assert_eq!(
            restarted
                .observe_swap_bridge(
                    fixture.operation,
                    attempt,
                    &clients,
                    Some(&bnb),
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            if attempt == traded_uid {
                expected.clone()
            } else {
                None
            }
        );
    }
    assert!(only_older_asked());
    restarted.shutdown().await;
    stub.abort();
    fixture.finish().await;
}

/// Routine bridge tracking of `order` by `owner`.
async fn track_bridge(
    owner: &ExecutorOwner,
    operation: ExecutorOperationId,
    order: OrderUid,
    clients: &crate::SwapBridgeClients,
    destination: &crate::settings::EffectiveChainConfig,
) -> Option<SwapBridgeOutcome> {
    owner
        .observe_swap_bridge(
            operation,
            order,
            clients,
            Some(destination),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
}

// An open order and its speed-up each sign their own NEAR Intents deposit address and quote.
// Whichever fills, the hand-off, the provider tracking and the outcome use the filled order's
// address and terms, and 1Click's report about the other address records nothing.
#[tokio::test]
async fn near_intents_tracking_follows_whichever_live_order_fills() {
    let destination_tx = B256::repeat_byte(2);
    let bnb =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(56)
            .cloned()
            .unwrap();
    let delivery = SwapDelivery::Bridge(BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 56,
        receiver: Address::repeat_byte(0x77),
        destination_token: Address::ZERO,
        surplus: BridgeSurplus::BridgedByProvider,
        private: None,
    });
    let addresses = [Address::repeat_byte(0x79), Address::repeat_byte(0x69)];
    let amounts_out = [16_u64, 18];
    let terms = |index: usize| {
        BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
            deposit_address: addresses[index],
            min_amount_out: U256::from(amounts_out[index] - 1),
            amount_out: U256::from(amounts_out[index]),
            deadline: format!("2026-09-30T0{}:00:00.000Z", index + 1),
            signed_quote: format!(r#"{{"quote":"{index}"}}"#),
        })
    };
    let uids = [uid(1, 100), uid(2, 110)];
    for filled in [0_usize, 1] {
        let unfilled = 1 - filled;
        let fixture = Fixture::start().await;
        fixture.record_delivery_attempt(1, 100, BUY, delivery.clone(), Some(terms(0)));
        record_bridge_speed_up(&fixture, 2, 110, uids[0], terms(1));

        // A finalized settlement holds the filled order's trade, which pays its address.
        let logs = vec![
            (fixture.settlement, trade_log(uids[filled])),
            (
                BUY,
                Transfer {
                    from: fixture.settlement,
                    to: addresses[filled],
                    value: U256::from(BUY_AMOUNT),
                }
                .encode_log_data(),
            ),
        ];
        let traded = settle_retry(&fixture, uids[filled], 30, logs)
            .await
            .unwrap();
        let outcomes = |record: &ExecutorRecord| {
            let orders = record.swap().unwrap().orders();
            (
                orders[filled].observations().bridge_outcome,
                orders[unfilled].observations().bridge_outcome,
            )
        };
        let record = fixture.record();
        let orders = record.swap().unwrap().orders();
        let (won, other) = (
            orders[filled].observations(),
            orders[unfilled].observations(),
        );
        assert_eq!(
            (won.traded, won.bridge_handoff, won.delivered),
            (
                Some(traded),
                Some(SwapBridgeHandoff {
                    observation: traded,
                    deposit_id: None,
                }),
                Some(traded)
            ),
            "filled {filled}"
        );
        assert_eq!(
            (other.traded, other.bridge_handoff, other.delivered),
            (None, None, None),
            "filled {filled}"
        );
        let tracked = record.swap_bridges_to_track().collect::<Vec<_>>();
        assert_eq!(
            tracked
                .iter()
                .map(|order| (order.uid(), order.bridge().cloned()))
                .collect::<Vec<_>>(),
            vec![(uids[filled], Some(terms(filled)))],
            "filled {filled}: tracking carries the filled order's address and signed quote"
        );

        // 1Click reports the filled order's deposit delivered and the other address refunded.
        let asked = addresses.map(|address| address.to_string());
        let answers = asked.clone();
        let delivered = amounts_out[filled];
        let (url, requests, stub) = crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub_with(move |path, _| {
            if path.contains(&answers[filled]) {
                (
                    200,
                    format!(
                        r#"{{"status":"SUCCESS","swapDetails":{{"amountOut":"{delivered}","destinationChainTxHashes":[{{"hash":"{destination_tx}","explorerUrl":"https://bscscan.com/tx/{destination_tx}"}}]}}}}"#
                    ),
                )
            } else if path.contains(&answers[unfilled]) {
                (200, r#"{"status":"REFUNDED"}"#.to_owned())
            } else {
                (400, "{}".to_owned())
            }
        })
        .await;
        let clients = bridge_clients(&fixture, &url);
        let expected = Some(SwapBridgeOutcome::DeliveredReported {
            amount_out: Some(U256::from(delivered)),
            transaction_hash: Some(destination_tx),
            destination_transaction: None,
        });
        let operation = fixture.operation;
        assert_eq!(
            track_bridge(&fixture.owner, operation, uids[filled], &clients, &bnb).await,
            expected,
            "filled {filled}"
        );
        assert_eq!(
            outcomes(&fixture.record()),
            (expected.clone(), None),
            "filled {filled}"
        );
        let only_filled_asked = || {
            let requests = requests.lock().unwrap();
            requests.len() == 1
                && requests.iter().all(|(path, _)| {
                    path.contains(&asked[filled]) && !path.contains(&asked[unfilled])
                })
        };
        assert!(only_filled_asked(), "filled {filled}");

        // The unfilled order holds no hand-off, so its address's refund or expiry is neither
        // recorded nor asked for, and can't replace the outcome.
        assert!(
            matches!(
                fixture.store.record_swap_bridge_outcome(
                    fixture.operation,
                    uids[unfilled],
                    SwapBridgeOutcome::Refunding
                ),
                Err(ExecutorStoreError::InvalidRecord)
            ),
            "filled {filled}"
        );
        assert_eq!(
            track_bridge(&fixture.owner, operation, uids[unfilled], &clients, &bnb).await,
            None,
            "filled {filled}"
        );
        assert!(only_filled_asked(), "filled {filled}");

        // A restart keeps the hand-off and the outcome on the filled order, and asks 1Click
        // nothing more.
        let restarted = restarted_owner(&fixture);
        let record = stored(&fixture);
        assert_eq!(
            outcomes(&record),
            (expected.clone(), None),
            "filled {filled}"
        );
        assert_eq!(record.swap_bridges_to_track().count(), 0, "filled {filled}");
        assert_eq!(
            swap_attempt_state(&record, &record.swap().unwrap().orders()[filled]),
            SwapOrderState::Done,
            "filled {filled}"
        );
        assert_eq!(
            (
                track_bridge(&restarted, operation, uids[filled], &clients, &bnb).await,
                track_bridge(&restarted, operation, uids[unfilled], &clients, &bnb).await
            ),
            (expected, None),
            "filled {filled}"
        );
        assert!(only_filled_asked(), "filled {filled}");
        restarted.shutdown().await;
        stub.abort();
        fixture.finish().await;
    }
}
