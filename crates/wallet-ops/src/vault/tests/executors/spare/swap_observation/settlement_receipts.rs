use super::*;

#[tokio::test]
async fn pending_observations_release_activity_and_never_overwrite_concurrent_changes() {
    // Exercise the account read, receipt-only settlement and a settled account's reuse
    // refresh: none may hold activity over RPC, and none may overwrite a changed local record.
    // The account read is a fact about the chain and is applied to the record as changed;
    // the other two decide from their snapshot and are refused.
    for path in ["account", "settlement", "settled reuse"] {
        let fixture = Fixture::start().await;
        fixture.record_attempt(1, 20);
        let logs = settlement_logs(&fixture, 1, 20);
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
        }
        if path == "settled reuse" {
            // The order is settled history and both of its hooks' nonces are consumed.
            fixture
                .owner
                .observe_swap_settlement(
                    fixture.operation,
                    uid(1, 20),
                    15,
                    crate::OperationHttpClient::direct_for_tests(),
                )
                .await
                .unwrap();
            fixture.chain.lock().unwrap().nonces.push((15, 3));
        }
        let gate = crate::rpc_broker::tests::RpcMockGate {
            request_started: Arc::default(),
            release_response: Arc::default(),
        };
        let served = fixture.chain.clone();
        let (endpoint, server) = crate::rpc_broker::tests::spawn_gated_rpc_mock(
            Arc::new(move |request| served.lock().unwrap().respond(&request)),
            Arc::default(),
            Arc::default(),
            gate.clone(),
        )
        .await;
        let mut config = fixture.config.clone();
        config.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
        let owner = ExecutorOwner::new(
            1,
            fixture.db.clone(),
            fixture.view.clone(),
            config,
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        let operation = fixture.operation;
        let pending = async {
            match path {
                "settlement" => {
                    owner
                        .observe_swap_settlement(
                            operation,
                            uid(1, 20),
                            15,
                            crate::OperationHttpClient::direct_for_tests(),
                        )
                        .await
                }
                "settled reuse" => owner
                    .reuse_swap_account(
                        operation,
                        15,
                        SwapAccountRole::Source,
                        SwapAccountUse::New,
                        crate::OperationHttpClient::direct_for_tests(),
                    )
                    .await
                    .map(|_| ()),
                _ => owner
                    .reconcile_account(operation, crate::OperationHttpClient::direct_for_tests())
                    .await
                    .map(|_| ()),
            }
        };
        tokio::pin!(pending);
        tokio::select! {
            () = gate.request_started.notified() => {},
            result = &mut pending => panic!("observation did not wait: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(5)) => panic!("observation never started"),
        }
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                owner.reconcile_account(
                    ExecutorOperationId::random().unwrap(),
                    crate::OperationHttpClient::direct_for_tests()
                ),
            )
            .await
            .expect("pending observation must release activity")
            .is_err()
        );
        owner.set_hidden(fixture.operation, true).unwrap();
        let changed = fixture.record();
        gate.release_response.notify_one();
        let release = tokio::spawn(async move {
            loop {
                gate.request_started.notified().await;
                gate.release_response.notify_one();
            }
        });
        let outcome = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap();
        if path == "account" {
            outcome.unwrap();
            let record = fixture.record();
            assert!(record.is_hidden(), "the read kept the concurrent change");
            assert_eq!(
                record.nonce_observation(),
                Some(ExecutorNonceObservation::new(
                    fixture.chain.lock().unwrap().block(15),
                    U256::ONE
                ))
            );
        } else {
            let error = outcome.unwrap_err();
            assert!(error.is::<crate::ExecutorRecordChanged>(), "{error:#}");
            assert_eq!(
                fixture.record(),
                changed,
                "late observation changed the record"
            );
        }
        release.abort();
        server.abort();
        owner.shutdown().await;
        fixture.finish().await;
    }
}

#[tokio::test]
async fn settlement_receipts_confirm_at_safety_depth_without_account_queries() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(1, 20);
    let logs = settlement_logs(&fixture, 1, 20);
    let nonce_before = fixture.record().nonce_observation();
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 15;
        // Another transaction in the block, whose gas isn't the settlement's.
        chain.add_addressed_transaction(15, Address::repeat_byte(0x55), Bytes::new(), Vec::new());
        let other = &mut chain.transactions.last_mut().unwrap().2;
        (other.gas_used, other.effective_gas_price) = (21_000, 9);
        chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
        let receipt = &mut chain.transactions.last_mut().unwrap().2;
        (receipt.gas_used, receipt.effective_gas_price) = (187_654, 2_345_678_901);
        chain.rpc_requests.clear();
    }
    // CoW can locate an unconfirmed block but cannot advance canonical progress.
    fixture
        .owner
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(
        fixture.record().swap().unwrap().orders()[0]
            .observations()
            .traded
            .is_none()
    );
    fixture.chain.lock().unwrap().head = 16;
    fixture
        .owner
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let record = fixture.record();
    let order = &record.swap().unwrap().orders()[0];
    assert_eq!(
        swap_order_state(record.swap().unwrap(), order),
        SwapOrderState::Done
    );
    assert_eq!(
        order
            .observations()
            .settlement_credit
            .unwrap()
            .private_amount,
        U256::from(BUY_AMOUNT - 24)
    );
    assert!(
        order.observations().shielded.is_none(),
        "receipt credit does not invent hook nonce evidence"
    );
    // The settlement's gas comes from the receipt that emits the trade, without another read.
    let amounts = order.observations().trade_amounts.unwrap();
    assert_eq!(
        (
            amounts.settlement_gas_used,
            amounts.settlement_effective_gas_price
        ),
        (Some(187_654), Some(2_345_678_901))
    );
    assert_eq!(record.nonce_observation(), nonce_before);
    assert!(
        fixture.reserved(&record),
        "spent inputs stay reserved until private sync catches up"
    );
    {
        let chain = fixture.chain.lock().unwrap();
        let block = BlockNumHash::new(15, chain.transactions.last().unwrap().1.block_hash.unwrap());
        assert_block_only_requests(&chain.rpc_requests, block);
    }
    // Persistence is sufficient after restart, and repeated confirmation reads no RPC.
    let reopened = ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1).unwrap();
    let restored = reopened
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == fixture.operation)
        .unwrap();
    assert_eq!(state(&restored, 0), SwapOrderState::Done);
    fixture.chain.lock().unwrap().rpc_requests.clear();
    fixture
        .owner
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(fixture.chain.lock().unwrap().rpc_requests, [] as [Value; 0]);
    // Reuse after restart keeps completed order evidence local. A fresh current nonce
    // must clear both old hooks, but we need not attribute their historical execution.
    let restarted = restarted_owner(&fixture);
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 17;
        chain.nonces.push((15, 2));
    }
    let reuse = |role| {
        restarted.reuse_swap_account(
            fixture.operation,
            16,
            role,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };
    // A stale nonce, still at the post-hook's, can't refresh the settled account.
    assert!(reuse(SwapAccountRole::Source).await.is_err());
    assert_eq!(fixture.record().nonce_observation(), nonce_before);
    fixture.chain.lock().unwrap().nonces.push((15, 3));
    fixture.chain.lock().unwrap().rpc_requests.clear();
    let reused = reuse(SwapAccountRole::Source).await.unwrap();
    assert_eq!(reused.expected_pre_hook_nonce(), U256::from(3));
    let checked = fixture.record();
    assert!(
        restarted
            .inputs_for_record(vec![sell_note(3)], &checked)
            .unwrap()
            .is_empty(),
        "completed swap inputs stay unavailable while private sync catches up"
    );
    assert!(
        checked.swap().unwrap().orders()[0]
            .observations()
            .shielded
            .is_none(),
        "current nonce admission must not fabricate a historical hook winner"
    );
    // The settled account's former role doesn't bind it: it is listed and admitted as another
    // swap's destination through the same refresh, with its order history kept.
    assert!(
        restarted
            .swap_destination_candidates(OTHER_BUY)
            .unwrap()
            .iter()
            .any(|candidate| candidate.operation() == fixture.operation)
    );
    let received = reuse(SwapAccountRole::Destination { token: OTHER_BUY })
        .await
        .unwrap();
    assert_eq!(
        received.delegated().unwrap().observed().nonce(),
        U256::from(3)
    );
    assert_eq!(fixture.record().swap(), checked.swap());
    {
        let chain = fixture.chain.lock().unwrap();
        assert_ne!(chain.rpc_requests, [] as [Value; 0]);
        for request in &chain.rpc_requests {
            match request["method"].as_str().unwrap() {
                "eth_chainId" | "eth_blockNumber" | "eth_getCode" => {}
                "eth_getBlockByNumber" => assert_eq!(quantity(&request["params"][0]), 16),
                "eth_call" => {
                    assert_eq!(request["params"][0]["to"], json!(EXECUTOR));
                    let input = request["params"][0]["input"]
                        .as_str()
                        .or_else(|| request["params"][0]["data"].as_str())
                        .unwrap();
                    assert_eq!(
                        input,
                        alloy::hex::encode_prefixed(RelayAdapt7702::nonceCall::SELECTOR)
                    );
                }
                method => panic!(
                    "completed orders must not trigger historical or identifying order reads: {method}"
                ),
            }
        }
    }
    fixture.record_attempt(2, 30);
    assert_eq!(
        fixture.record().swap().unwrap().orders()[1]
            .pre_hook()
            .nonce(),
        U256::from(3)
    );
    restarted.shutdown().await;
    fixture.finish().await;
}

#[tokio::test]
async fn the_executed_fee_is_read_once_after_the_trade_and_kept_by_later_settlement_checks() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(1, 20);
    let order = uid(1, 20);
    let (url, requests, stub) =
        crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(|_| {
            json!({"status": "fulfilled", "executedFee": "464572", "executedFeeToken": BUY})
                .to_string()
        })
        .await;
    let orderbook = crate::cow::CowOrderbookClient::new(
        crate::OperationHttpClient::for_tests(
            reqwest::Client::new(),
            crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
        ),
        url,
        1,
    )
    .unwrap();
    let fee = |record: &ExecutorRecord| {
        record.swap().unwrap().orders()[0]
            .observations()
            .trade_amounts
            .map(|amounts| (amounts.executed_fee, amounts.executed_fee_token))
    };

    // Nothing is asked before the trade is verified.
    fixture
        .owner
        .observe_swap_executed_fee(fixture.operation, order, &orderbook)
        .await
        .unwrap();
    assert!(requests.lock().unwrap().is_empty());

    // A credit below the approved minimum leaves the order traded, so its settlement is
    // checked again later.
    let mut logs = settlement_logs(&fixture, 1, 20);
    logs[4].1 = post_hook_shield_log(1, 10);
    confirm_settlement(&fixture, logs).await;
    assert_eq!(state(&fixture.record(), 0), SwapOrderState::Traded);
    fixture
        .owner
        .observe_swap_executed_fee(fixture.operation, order, &orderbook)
        .await
        .unwrap();
    let charged = Some((Some(U256::from(464_572)), Some(BUY)));
    assert_eq!(fee(&fixture.record()), charged);
    assert_eq!(
        *requests.lock().unwrap(),
        vec![(format!("/api/api/v1/orders/{}", order.0), Value::Null)],
        "the fee read carries only the order UID"
    );

    // The next settlement check records the same trade again and keeps the fee, and neither
    // this owner nor one after a restart asks again.
    fixture
        .owner
        .observe_swap_settlement(
            fixture.operation,
            order,
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(fee(&fixture.record()), charged);
    let restarted = restarted_owner(&fixture);
    for owner in [&fixture.owner, &restarted] {
        owner
            .observe_swap_executed_fee(fixture.operation, order, &orderbook)
            .await
            .unwrap();
    }
    assert_eq!(requests.lock().unwrap().len(), 1);
    stub.abort();
    restarted.shutdown().await;
    fixture.finish().await;
}

#[tokio::test]
async fn settlement_receipts_do_not_complete_an_early_or_copied_or_small_shield() {
    for scenario in 0..6 {
        let fixture = Fixture::start().await;
        fixture.record_attempt(1, 20);
        let mut logs = settlement_logs(&fixture, 1, 20);
        match scenario {
            0 => logs.rotate_right(3), // Funded post-hook before the trade and payout.
            1 => {
                logs[2].1 = Transfer {
                    from: Address::repeat_byte(0xab),
                    to: fixture.railgun,
                    value: U256::from(BUY_AMOUNT - 24),
                }
                .encode_log_data();
            }
            2 => logs[4].1 = post_hook_shield_log(1, 10), // Below the approved private minimum.
            3 => logs.insert(
                4,
                (fixture.railgun, post_hook_shield_log(2, BUY_AMOUNT - 24)),
            ), // Debit belonged to a different shield.
            4 => logs.push(logs[1].clone()), // Early funded hook followed by the actual payout.
            5 => {
                // A debit for the gross amount does not match this net private credit.
                logs[2].1 = Transfer {
                    from: EXECUTOR,
                    to: fixture.railgun,
                    value: U256::from(BUY_AMOUNT + 1),
                }
                .encode_log_data();
            }
            _ => unreachable!(),
        }
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
            chain.rpc_requests.clear();
        }
        fixture
            .owner
            .observe_swap_settlement(
                fixture.operation,
                uid(1, 20),
                15,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        let record = fixture.record();
        assert_eq!(
            state(&record, 0),
            SwapOrderState::Traded,
            "scenario {scenario}"
        );
        assert!(fixture.reserved(&record));
        {
            let chain = fixture.chain.lock().unwrap();
            let block =
                BlockNumHash::new(15, chain.transactions.last().unwrap().1.block_hash.unwrap());
            assert_block_only_requests(&chain.rpc_requests, block);
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn settlement_receipts_deliver_external_orders_from_the_trade_alone() {
    let receiver = Address::repeat_byte(0x77);
    let order = uid(1, 20);
    // An ERC-20 buy pays the receiver with a Transfer. A native buy's Trade reports `GPv2`'s
    // native buy address and emits no Transfer. Another order's trade settles nothing.
    for (buy, trade, matching) in [
        (BUY, trade_log_for(order, BUY), true),
        (Address::ZERO, trade_log_for(order, BUY_NATIVE_TOKEN), true),
        (BUY, trade_log_for(uid(2, 20), BUY), false),
    ] {
        let fixture = Fixture::start().await;
        fixture.record_delivery_attempt(1, 20, buy, SwapDelivery::External { receiver }, None);
        let mut logs = vec![(fixture.settlement, trade)];
        if buy == BUY {
            logs.push((
                BUY,
                Transfer {
                    from: fixture.settlement,
                    to: receiver,
                    value: U256::from(BUY_AMOUNT),
                }
                .encode_log_data(),
            ));
        }
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
            chain.rpc_requests.clear();
        }
        fixture
            .owner
            .observe_swap_settlement(
                fixture.operation,
                order,
                15,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        let record = fixture.record();
        let observed = record.swap().unwrap().orders()[0].observations();
        {
            let chain = fixture.chain.lock().unwrap();
            let block = chain.block(15);
            let traded = SwapObservation {
                block,
                transaction_hash: Some(chain.transactions.last().unwrap().0),
            };
            if matching {
                assert_eq!(state(&record, 0), SwapOrderState::Done);
                assert_eq!(
                    (observed.traded, observed.delivered),
                    (Some(traded), Some(traded))
                );
                assert!(observed.settlement_credit.is_none());
            } else {
                assert_eq!(observed, SwapOrderObservations::default());
            }
            assert_block_only_requests(&chain.rpc_requests, block);
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn settlement_receipts_hand_off_to_across_only_with_the_signed_deposit() {
    // 0: the signed deposit, then the reshielded surplus. 1 and 2: a deposit to another
    // recipient, or of another output amount. 3: the post-hook didn't run. 4: the signed
    // deposit, paid by someone other than the executor. 5: a private order's signed deposit,
    // which pays the handler with the signed message. 6: that deposit with another message.
    for scenario in 0..7 {
        let fixture = Fixture::start().await;
        let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
        let (delivery, terms) = if scenario >= 5 {
            private_across_order(spoke_pool, Address::repeat_byte(0x7a))
        } else {
            across_order(spoke_pool)
        };
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let mut deposit = signed_deposit(delivery, &terms);
        match scenario {
            1 => deposit.recipient = address_to_bytes32(Address::repeat_byte(0x78)),
            2 => deposit.outputAmount -= U256::ONE,
            6 => deposit.message = Bytes::from_static(b"another message"),
            _ => {}
        }
        let mut logs = across_settlement_logs(&fixture, &terms, (scenario != 3).then_some(deposit));
        if scenario == 4 {
            let executor_payment = Transfer {
                from: EXECUTOR,
                to: terms.spoke_pool,
                value: terms.input_amount,
            }
            .encode_log_data();
            logs.retain(|(_, data)| *data != executor_payment);
        }
        let traded = confirm_settlement(&fixture, logs).await;
        let record = fixture.record();
        let observed = record.swap().unwrap().orders()[0].observations();
        assert_eq!(observed.traded, Some(traded), "scenario {scenario}");
        if !matches!(scenario, 0 | 5) {
            // The bought token stays in the executor.
            assert_eq!(
                state(&record, 0),
                SwapOrderState::Traded,
                "scenario {scenario}"
            );
            assert_eq!(
                (
                    observed.bridge_handoff,
                    observed.delivered,
                    observed.settlement_credit
                ),
                (None, None, None),
                "scenario {scenario}"
            );
            if scenario == 3 {
                // Explicit reconciliation finds the bought token still in the executor, which
                // recovery then offers.
                {
                    let mut chain = fixture.chain.lock().unwrap();
                    chain.nonces.push((15, 2));
                    chain.buy_balance.push((15, BUY_AMOUNT));
                }
                let record = fixture.observe(17, 14).await;
                assert_eq!(state(&record, 0), SwapOrderState::NotDelivered);
            }
            fixture.finish().await;
            continue;
        }
        // Handed off, and not yet delivered on the destination chain.
        assert_eq!(state(&record, 0), SwapOrderState::Bridging);
        assert_eq!(
            (observed.bridge_handoff, observed.delivered),
            (
                Some(SwapBridgeHandoff {
                    observation: traded,
                    deposit_id: Some(U256::from(42)),
                }),
                Some(traded)
            ),
            "scenario {scenario}"
        );
        assert_eq!(
            observed
                .settlement_credit
                .map(|credit| (credit.private_amount, credit.fee)),
            Some((U256::from(74), Some(U256::from(25))))
        );
        // Explicit reconciliation finds the post-hook's nonce passed in the settlement, so the
        // retained deposit shows which payload took that nonce.
        fixture.chain.lock().unwrap().nonces.push((15, 3));
        let record = fixture.observe(17, 14).await;
        assert_eq!(
            record.swap().unwrap().orders()[0]
                .observations()
                .post_hook_deposit,
            Some(traded)
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn an_unconfirmed_settlement_block_hints_what_it_shows_and_records_nothing() {
    // 0: an Across order's signed deposit. 1: its trade without a deposit. 2: the signed
    // deposit, on an endpoint without whole-block receipts. 3: a Reshield order's trade and
    // shield.
    for scenario in 0..4 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
        let logs = if scenario == 3 {
            fixture.record_attempt(1, 20);
            settlement_logs(&fixture, 1, 20)
        } else {
            fixture.record_delivery_attempt(
                1,
                20,
                BUY,
                SwapDelivery::Bridge(delivery),
                Some(BridgeOrderTerms::Across(terms)),
            );
            let deposit = (scenario != 1).then(|| signed_deposit(delivery, &terms));
            across_settlement_logs(&fixture, &terms, deposit)
        };
        // Block 15 is the head, short of the finality depth of 1.
        let (block, settlement) = {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 15;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
            if scenario == 2 {
                chain.receipt_error = Some(-32601);
            }
            chain.rpc_requests.clear();
            (chain.block(15), chain.transactions.last().unwrap().0)
        };
        let before = fixture.record();
        let hint = fixture
            .owner
            .read_swap_settlement_hint(
                fixture.operation,
                uid(1, 20),
                15,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(
            hint,
            (scenario != 2).then_some(crate::SwapSettlementHint {
                block,
                bought: U256::from(BUY_AMOUNT),
                // The Across order's reshielded surplus is not reported as its shield.
                shielded: (scenario == 3).then_some(U256::from(BUY_AMOUNT - 24)),
                shield_transaction: (scenario == 3).then_some(settlement),
                handoff: (scenario == 0).then_some(crate::SwapHandoffHint {
                    block,
                    deposit_id: Some(U256::from(42)),
                    sent: terms.input_amount,
                }),
            }),
            "scenario {scenario}"
        );
        assert_eq!(fixture.record(), before, "scenario {scenario}");
        assert_block_only_requests(&fixture.chain.lock().unwrap().rpc_requests, block);
        fixture.finish().await;
    }
}

// An expired attempt's Across approval stays for a retry at the same nonce until one finalized
// block shows both the unused nonce and a timestamp past the signed quote's age limit, read
// from the pool at that block. Without that evidence the approval stays.
#[tokio::test]
async fn continuing_approvals_read_the_quote_age_from_one_finalized_block() {
    let fixture = Fixture::start().await;
    let spoke_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
    let (delivery, older) = across_order(spoke_pool);
    expired_across_attempt(&fixture, 1, 20, delivery, older).await;
    let swap_use = fixture.record().swap().unwrap().orders()[0]
        .use_id()
        .unwrap();
    // The lower minimum a retry approving 9,900 at `nonce` must disclose.
    let (owner, operation) = (&fixture.owner, fixture.operation);
    let continuing = move |nonce: u64| async move {
        owner
            .swap_continuing_approvals(
                operation,
                swap_use,
                &SwapDelivery::Bridge(delivery),
                Some(U256::from(nonce)),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .lower_minimum(U256::from(9_900))
    };

    // The pool's limit can't be read.
    assert_eq!(continuing(1).await, Some(older.output_amount));

    // The finalized block is exactly at the limit, which still permits a deposit.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let boundary = timestamp(chain.head - 1) - u64::from(older.quote_timestamp);
        chain.quote_time_buffer = Some(u32::try_from(boundary).unwrap());
        chain.rpc_requests.clear();
    }
    assert_eq!(continuing(1).await, Some(older.output_amount));
    {
        let chain = fixture.chain.lock().unwrap();
        let mut called = Vec::new();
        for request in &chain.rpc_requests {
            match request["method"].as_str().unwrap() {
                "eth_blockNumber" | "eth_getBlockByNumber" | "eth_getCode" => {}
                "eth_call" => called.push(
                    serde_json::from_value::<Address>(request["params"][0]["to"].clone()).unwrap(),
                ),
                method => panic!("the quote-age read sent {method}"),
            }
        }
        called.sort();
        called.dedup();
        let mut expected = vec![EXECUTOR, spoke_pool];
        expected.sort();
        assert_eq!(called, expected);
    }

    // One finalized block later the quote is too old, with the post-hook's nonce still unused.
    fixture.chain.lock().unwrap().head += 1;
    assert_eq!(continuing(1).await, None);

    // A retry at the old post-hook's nonce passes it, which needs no evidence.
    fixture.chain.lock().unwrap().rpc_requests.clear();
    assert_eq!(continuing(2).await, None);
    assert_eq!(fixture.chain.lock().unwrap().rpc_requests, [] as [Value; 0]);
    fixture.finish().await;
}

#[tokio::test]
async fn settlement_receipt_failures_and_reorgs_preserve_pending_state_without_fallback() {
    for scenario in 0..5 {
        let fixture = Fixture::start().await;
        fixture.record_attempt(1, 20);
        let mut logs = settlement_logs(&fixture, 1, 20);
        if scenario == 4 {
            logs[0].1 = trade_log(uid(2, 20));
        }
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 16;
            chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
            match scenario {
                0 => chain.receipt_error = Some(-32601),
                1 => chain.transactions.last_mut().unwrap().2.block_hash = Some(B256::ZERO),
                2 => chain.reorg_on_receipts = true,
                3 => {
                    chain.transactions.last_mut().unwrap().2.inner = ReceiptEnvelope::Eip1559(
                        Receipt {
                            status: Eip658Value::Eip658(false),
                            cumulative_gas_used: 1,
                            logs: Vec::new(),
                        }
                        .with_bloom(),
                    );
                }
                4 => {}
                _ => unreachable!(),
            }
            chain.rpc_requests.clear();
        }
        let result = fixture
            .owner
            .observe_swap_settlement(
                fixture.operation,
                uid(1, 20),
                15,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await;
        assert_eq!(result.is_err(), scenario < 3);
        let record = fixture.record();
        assert!(
            record.swap().unwrap().orders()[0]
                .observations()
                .traded
                .is_none()
        );
        assert!(fixture.reserved(&record));
        {
            let chain = fixture.chain.lock().unwrap();
            let block =
                BlockNumHash::new(15, chain.transactions.last().unwrap().1.block_hash.unwrap());
            assert_block_only_requests(&chain.rpc_requests, block);
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn settlement_receipts_fail_over_to_another_whole_block_provider() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(1, 20);
    let logs = settlement_logs(&fixture, 1, 20);
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.add_addressed_transaction(15, fixture.settlement, Bytes::new(), logs);
        chain.rpc_requests.clear();
    }
    let failed_receipts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failed = failed_receipts.clone();
    let served = fixture.chain.clone();
    let (unsupported, unsupported_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            let result = served.lock().unwrap().respond(&request);
            if request["method"] == "eth_getBlockReceipts" {
                failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":-32601, "message":"unsupported"}})
            } else { result }
        }), Arc::default(), Arc::default(),
    ).await;
    let served = fixture.chain.clone();
    let (working, working_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| served.lock().unwrap().respond(&request)),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let mut config =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    config.finality_depth = 1;
    config.rpc_route = crate::RpcChainRoute::new(1, vec![unsupported, working]);
    let owner = ExecutorOwner::new(
        0,
        fixture.db.clone(),
        fixture.view.clone(),
        config,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    owner
        .observe_swap_settlement(
            fixture.operation,
            uid(1, 20),
            15,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(
        failed_receipts.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(state(&fixture.record(), 0), SwapOrderState::Done);
    {
        let chain = fixture.chain.lock().unwrap();
        assert_block_only_requests(&chain.rpc_requests, chain.block(15));
    }
    owner.shutdown().await;
    unsupported_server.abort();
    working_server.abort();
    fixture.finish().await;
}
