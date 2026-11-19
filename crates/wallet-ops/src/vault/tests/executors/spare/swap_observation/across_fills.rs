use super::*;

#[tokio::test]
async fn across_delivery_is_verified_from_finalized_destination_receipts() {
    // 0: the deposit's fill. 1: a fill of another deposit. 2: the fill's block is reorged
    // during the read. 3: Across reports the deposit expired. 4: a slow fill, which pays more
    // than the signed output. 5: a fill that executes less than the signed output.
    // A private order, whose record without a message is scenario 0: 6: the fill, the
    // handler's transfer to the destination stealth account and its shield. 7: the fill and
    // that transfer, without the shield. 8: a fill executed with another message. 9: Across
    // reports the deposit expired. 10: the fill, which doesn't reach the destination stealth
    // account.
    for scenario in 0..11 {
        let fixture = Fixture::start().await;
        let (destination, config, server) = across_destination(42161).await;
        let profile = config.bridge_profile().unwrap();
        let (spoke_pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
        let railgun = config.require_railgun().unwrap().deployment.contract;
        let origin_pool = fixture.config.bridge_profile().unwrap().spoke_pool();
        let private = scenario >= 6;
        let (delivery, terms) = if private {
            private_across_order(origin_pool, handler)
        } else {
            across_order(origin_pool)
        };
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let logs = across_settlement_logs(&fixture, &terms, Some(signed_deposit(delivery, &terms)));
        confirm_settlement(&fixture, logs).await;
        let mut fill = filled_relay(delivery, &terms, if scenario == 1 { 43 } else { 42 });
        let executed = match scenario {
            4 => {
                fill.relayExecutionInfo.fillType = 2;
                terms.output_amount + U256::from(50)
            }
            5 => terms.output_amount - U256::ONE,
            _ => terms.output_amount,
        };
        fill.relayExecutionInfo.updatedOutputAmount = executed;
        if scenario == 8 {
            fill.relayExecutionInfo.updatedMessageHash = B256::ZERO;
        }
        // The handler passes its whole balance on, which can exceed the fill.
        let received = executed + U256::from(7);
        let transfer = |from: Address, to: Address, value: U256| {
            (
                terms.output_token,
                Transfer { from, to, value }.encode_log_data(),
            )
        };
        let mut logs = vec![(spoke_pool, fill.encode_log_data())];
        if private && scenario != 10 {
            logs.push(transfer(handler, delivery.receiver, received));
            if scenario != 7 {
                // The shield moves the net amount to Railgun and the fee to its treasury.
                logs.extend([
                    transfer(delivery.receiver, railgun, received - U256::from(25)),
                    transfer(
                        delivery.receiver,
                        Address::repeat_byte(0xfe),
                        U256::from(25),
                    ),
                ]);
            }
        }
        {
            let mut chain = destination.lock().unwrap();
            chain.add_addressed_transaction(30, spoke_pool, Bytes::new(), logs);
            chain.reorg_on_receipts = scenario == 2;
        }
        // Across's fill transaction, amount and recipient only locate the block.
        let expired = matches!(scenario, 3 | 9);
        let status = if expired { "expired" } else { "filled" };
        let deposit = format!(
            r#"{{"deposit":{{"status":"{status}","fillBlockNumber":30,"fillTx":"{}","outputAmount":"1","recipient":"{}","destinationChainId":"42161"}}}}"#,
            B256::repeat_byte(0xf1),
            Address::repeat_byte(0x78),
        );
        let (url, lookups, stub) =
            crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(move |_| {
                deposit.clone()
            })
            .await;
        let clients = bridge_clients(&fixture, &url);
        let first = observe_bridge(&fixture, &clients, &config).await;
        let outcome = if expired {
            // The destination chain isn't read.
            assert_eq!(first, Some(SwapBridgeOutcome::Refunding));
            assert_eq!(destination.lock().unwrap().rpc_requests, [] as [Value; 0]);
            first
        } else {
            // The fill's block isn't final at head 30.
            assert_eq!(first, None, "scenario {scenario}");
            destination.lock().unwrap().head = 31;
            let outcome = observe_bridge(&fixture, &clients, &config).await;
            let chain = destination.lock().unwrap();
            let (transaction_hash, transaction, _) = &chain.transactions[0];
            let block = BlockNumHash::new(30, transaction.block_hash.unwrap());
            let transaction_hash = *transaction_hash;
            // The executed amount is delivered. A private delivery records the amount the
            // destination stealth account received, shielded or held.
            let expected = match scenario {
                0 | 4 => Some(SwapBridgeOutcome::DeliveredVerified {
                    block,
                    transaction_hash,
                    output_amount: executed,
                    shielded: false,
                }),
                6 => Some(SwapBridgeOutcome::DeliveredVerified {
                    block,
                    transaction_hash,
                    output_amount: received,
                    shielded: true,
                }),
                7 => Some(SwapBridgeOutcome::HeldOnDestination {
                    block,
                    transaction_hash,
                    amount: received,
                }),
                // The final block holds no fill of the deposit, so a public delivery is
                // Across's word, with the amount and transaction it names.
                1 | 5 => Some(SwapBridgeOutcome::DeliveredReported {
                    amount_out: Some(U256::ONE),
                    transaction_hash: Some(B256::repeat_byte(0xf1)),
                    destination_transaction: None,
                }),
                // A private delivery is never reported.
                _ => None,
            };
            assert_eq!(outcome, expected, "scenario {scenario}");
            assert_block_only_requests(&chain.rpc_requests, block);
            outcome
        };
        if scenario == 10 {
            // Nor is it reported when no endpoint serves whole-block receipts.
            destination.lock().unwrap().receipt_error = Some(-32601);
            assert!(
                fixture
                    .owner
                    .observe_swap_bridge(
                        fixture.operation,
                        uid(1, 20),
                        &clients,
                        Some(&config),
                        crate::OperationHttpClient::direct_for_tests()
                    )
                    .await
                    .is_err()
            );
        }
        let record = fixture.record();
        assert_eq!(
            record.swap().unwrap().orders()[0]
                .observations()
                .bridge_outcome,
            outcome
        );
        assert_eq!(
            state(&record, 0),
            [
                SwapOrderState::Done,
                SwapOrderState::Done,
                SwapOrderState::Bridging,
                SwapOrderState::Refunding,
                SwapOrderState::Done,
                SwapOrderState::Done,
                SwapOrderState::Done,
                SwapOrderState::HeldOnDestination,
                SwapOrderState::Bridging,
                SwapOrderState::Refunding,
                SwapOrderState::Bridging,
            ][scenario],
            "scenario {scenario}"
        );
        assert!(
            lookups
                .lock()
                .unwrap()
                .iter()
                .all(|(path, _)| path == "/api/deposit?originChainId=1&depositId=42")
        );
        stub.abort();
        server.abort();
        fixture.finish().await;
    }
}

/// Across can fill a deposit after reporting it expired. Routine polling stops at the refund,
/// but an explicit status check asks Across again and records the verified fill.
#[tokio::test]
async fn an_explicit_check_corrects_an_across_refund_with_a_verified_fill() {
    let fixture = Fixture::start().await;
    let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
    fixture.record_delivery_attempt(
        1,
        20,
        BUY,
        SwapDelivery::Bridge(delivery),
        Some(BridgeOrderTerms::Across(terms)),
    );
    let logs = across_settlement_logs(&fixture, &terms, Some(signed_deposit(delivery, &terms)));
    confirm_settlement(&fixture, logs).await;
    let (destination, config, server) = across_destination(42161).await;
    let spoke_pool = config.bridge_profile().unwrap().spoke_pool();
    {
        let mut chain = destination.lock().unwrap();
        chain.add_addressed_transaction(
            30,
            spoke_pool,
            Bytes::new(),
            vec![(
                spoke_pool,
                filled_relay(delivery, &terms, 42).encode_log_data(),
            )],
        );
        chain.head = 31;
    }
    let deposit = |status: &str| {
        format!(
            r#"{{"deposit":{{"status":"{status}","fillBlockNumber":30,"outputAmount":"1","recipient":"{}","destinationChainId":"42161"}}}}"#,
            delivery.receiver,
        )
    };
    let reply = Arc::new(Mutex::new(deposit("expired")));
    let served = reply.clone();
    let (url, lookups, stub) =
        crate::vault::tests::executors::spare::swap_order::spawn_bridge_stub(move |_| {
            served.lock().unwrap().clone()
        })
        .await;
    let clients = bridge_clients(&fixture, &url);
    assert_eq!(
        observe_bridge(&fixture, &clients, &config).await,
        Some(SwapBridgeOutcome::Refunding)
    );
    *reply.lock().unwrap() = deposit("filled");
    assert_eq!(
        observe_bridge(&fixture, &clients, &config).await,
        Some(SwapBridgeOutcome::Refunding)
    );
    assert_eq!(lookups.lock().unwrap().len(), 1);
    assert_eq!(fixture.record().swap_bridges_to_track().count(), 0);
    let outcome = fixture
        .owner
        .check_swap_bridge(
            fixture.operation,
            uid(1, 20),
            &clients,
            Some(&config),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let expected = {
        let chain = destination.lock().unwrap();
        let (transaction_hash, transaction, _) = &chain.transactions[0];
        SwapBridgeOutcome::DeliveredVerified {
            block: BlockNumHash::new(30, transaction.block_hash.unwrap()),
            transaction_hash: *transaction_hash,
            output_amount: terms.output_amount,
            shielded: false,
        }
    };
    assert_eq!(outcome, Some(expected.clone()));
    let record = fixture.record();
    assert_eq!(
        record.swap().unwrap().orders()[0]
            .observations()
            .bridge_outcome,
        Some(expected)
    );
    assert_eq!(state(&record, 0), SwapOrderState::Done);
    stub.abort();
    server.abort();
    fixture.finish().await;
}

/// Across refunds an expired deposit to the stealth account, which may also hold kept surplus
/// larger than the deposit, so its balance can't show the refund. Routine polling doesn't look
/// the refund up. An explicit status check verifies it in the finalized receipts of the block
/// holding the refund transaction Across names: a transfer of the deposit's input token from
/// the `SpokePool` to the stealth account covering the deposit. The verified refund survives a
/// restart and reconciliation, and a reorg of its block removes it.
#[tokio::test]
async fn an_explicit_check_verifies_an_across_refund_on_this_chain() {
    let refund_of = |record: &ExecutorRecord| {
        record.swap().unwrap().orders()[0]
            .observations()
            .bridge_refund
    };
    let arbitrum =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(42161)
            .cloned()
            .unwrap();
    // 0: the refund. 1: from another sender. 2: of another token. 3: to another account. 4: less
    // than the deposit.
    for scenario in 0..5 {
        let fixture = Fixture::start().await;
        let (delivery, terms) = across_order(fixture.config.bridge_profile().unwrap().spoke_pool());
        let delivery = BridgeDelivery {
            surplus: BridgeSurplus::KeepInAccount,
            ..delivery
        };
        let terms = AcrossOrderTerms {
            input_amount: U256::from(4_000),
            ..terms
        };
        fixture.record_delivery_attempt(
            1,
            20,
            BUY,
            SwapDelivery::Bridge(delivery),
            Some(BridgeOrderTerms::Across(terms)),
        );
        let mut logs =
            across_settlement_logs(&fixture, &terms, Some(signed_deposit(delivery, &terms)));
        // The post-hook keeps the surplus of 5,999 rather than shielding it.
        logs.truncate(logs.len() - 3);
        confirm_settlement(&fixture, logs).await;
        let mut refund = Transfer {
            from: terms.spoke_pool,
            to: EXECUTOR,
            value: terms.input_amount,
        };
        let mut token = BUY;
        match scenario {
            1 => refund.from = Address::repeat_byte(0x99),
            2 => token = OTHER_BUY,
            3 => refund.to = Address::repeat_byte(0x99),
            4 => refund.value -= U256::ONE,
            _ => {}
        }
        let refund_tx = {
            let mut chain = fixture.chain.lock().unwrap();
            chain.add_addressed_transaction(
                18,
                terms.spoke_pool,
                Bytes::new(),
                vec![(token, refund.encode_log_data())],
            );
            chain.head = 20;
            chain.rpc_methods.clear();
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
            observe_bridge(&fixture, &clients, &arbitrum).await,
            Some(SwapBridgeOutcome::Refunding)
        );
        let looked_up = |fixture: &Fixture| {
            fixture
                .chain
                .lock()
                .unwrap()
                .rpc_methods
                .iter()
                .any(|method| method == "eth_getTransactionReceipt")
        };
        assert!(!looked_up(&fixture));
        assert_eq!(refund_of(&fixture.record()), None);
        assert_eq!(
            fixture
                .owner
                .check_swap_bridge(
                    fixture.operation,
                    uid(1, 20),
                    &clients,
                    Some(&arbitrum),
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            Some(SwapBridgeOutcome::Refunding)
        );
        assert!(looked_up(&fixture));
        let verified = SwapObservation {
            block: fixture.chain.lock().unwrap().block(18),
            transaction_hash: Some(refund_tx),
        };
        let record = fixture.record();
        assert_eq!(
            refund_of(&record),
            (scenario == 0).then_some(verified),
            "scenario {scenario}"
        );
        assert_eq!(state(&record, 0), SwapOrderState::Refunding);
        if scenario == 0 {
            // A verified refund rules out a fill, and isn't replaced.
            assert!(matches!(
                fixture.store.record_swap_bridge_outcome(
                    fixture.operation,
                    uid(1, 20),
                    SwapBridgeOutcome::DeliveredVerified {
                        block: BlockNumHash::new(30, B256::repeat_byte(30)),
                        transaction_hash: B256::repeat_byte(31),
                        output_amount: terms.output_amount,
                        shielded: false,
                    },
                ),
                Err(ExecutorStoreError::InvalidRecord)
            ));
            assert!(matches!(
                fixture.store.record_swap_bridge_refund(
                    fixture.operation,
                    uid(1, 20),
                    SwapObservation {
                        block: fixture.chain.lock().unwrap().block(19),
                        ..verified
                    },
                ),
                Err(ExecutorStoreError::InvalidRecord)
            ));
            let restarted =
                ExecutorStore::new(fixture.db.clone(), fixture.view.clone(), 1).unwrap();
            let restored = restarted
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == fixture.operation)
                .unwrap();
            assert_eq!(refund_of(&restored), Some(verified));
            drop(restarted);
            // The post-hook's nonce passed in the settlement.
            fixture.chain.lock().unwrap().nonces.push((15, 3));
            assert_eq!(refund_of(&fixture.observe(20, 14).await), Some(verified));
            fixture.chain.lock().unwrap().reorg(18);
            assert_eq!(refund_of(&fixture.observe(21, 14).await), None);
        }
        stub.abort();
        fixture.finish().await;
    }
}
