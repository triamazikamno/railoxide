use super::*;

// An order's hook deposits more than the signed minimums, and the fill repeats the deposit's
// amounts. The destination chain's owner matches the fill on those: a fill of the signed
// minimums is not this deposit's. The matching fill, the handler's transfer to the destination
// account and its shield in one final receipt deliver the swap, and the account's shield
// payload is recorded as run in the same write.
#[tokio::test]
async fn a_public_swap_is_delivered_by_the_fill_of_its_deposited_amounts_and_its_shield() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let (across, reply, lookups, across_task) = across_stub().await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let observe = || {
        owner.observe_public_swap_bridge(
            payment.operation,
            payment.id,
            &across,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };

    // Without a hand-off there is nothing to ask Across about.
    assert_eq!(observe().await.unwrap(), None);
    assert!(lookups.lock().unwrap().is_empty());

    // The signed minimums are 1,010 and 1,000.
    let (input, output) = (U256::from(1_020), U256::from(1_009));
    payment.settle(uid, input, Some(&payment.deposited(input, output)));
    payment.mine();
    payment.observe(&owner, &orderbook).await;

    let (signed, _) = payment.fill(
        payment.terms.input_amount,
        payment.terms.output_amount,
        true,
    );
    *reply.lock().unwrap() = across_deposit("filled", Some(signed.number), None);
    assert_eq!(observe().await.unwrap(), None);
    assert_eq!(payment.saved().outcome, None);

    let (block, transaction_hash) = payment.fill(input, output, true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    let delivered = SwapBridgeOutcome::DeliveredVerified {
        block,
        transaction_hash,
        output_amount: output,
        shielded: true,
    };
    assert_eq!(observe().await.unwrap(), Some(delivered.clone()));
    let saved = payment.saved();
    assert_eq!(
        saved.swap.observations().bridge_outcome,
        Some(delivered.clone())
    );
    assert_eq!(
        saved.outcome,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );
    // Across was asked about the deposit on the chain the Public account pays on.
    let asked = lookups.lock().unwrap().len();
    assert!(lookups.lock().unwrap().iter().all(|(path, _)| {
        *path == format!("/api/deposit?originChainId={DESTINATION_CHAIN}&depositId={DEPOSIT_ID}")
    }));
    // A recorded delivery isn't asked about again.
    assert_eq!(observe().await.unwrap(), Some(delivered));
    assert_eq!(lookups.lock().unwrap().len(), asked);

    across_task.abort();
    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The fill completed and its receipt holds no shield: the destination account holds the token,
// and its shield payload can still run.
#[tokio::test(start_paused = true)]
async fn a_public_swaps_fill_without_its_shield_is_held_on_the_destination() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    payment.handed_off(&owner).await;

    let amount = U256::from(1_000);
    let (block, transaction_hash) = payment.fill(payment.sell_amount, amount, false);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    let held = SwapBridgeOutcome::HeldOnDestination {
        block,
        transaction_hash,
        amount,
    };
    assert_eq!(
        owner
            .observe_public_swap_bridge(
                payment.operation,
                payment.id,
                &across,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        Some(held.clone())
    );
    let saved = payment.saved();
    assert_eq!(saved.swap.observations().bridge_outcome, Some(held));
    assert_eq!(
        saved.outcome,
        Some(SwapDestinationOutcome::Held {
            block,
            transaction_hash,
        })
    );

    let balance_selector = alloy::hex::encode_prefixed(PublicErc20::balanceOfCall::SELECTOR);
    let balance_reads = |requests: &[Value]| {
        requests
            .iter()
            .filter(|request| {
                request["method"] == "eth_call"
                    && request["params"][0]
                        .get("input")
                        .or_else(|| request["params"][0].get("data"))
                        .and_then(Value::as_str)
                        .is_some_and(|input| input.starts_with(&balance_selector))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    {
        let mut chain = payment.delivery_chain.lock().unwrap();
        let head = chain.head;
        chain.buy_balance.extend([(block.number, 1_000), (head, 0)]);
        chain.rpc_requests.clear();
    }
    // Background tracking keeps the receipt's held outcome and does not disclose a balance.
    assert_eq!(
        payment
            .track(&owner, &across, None)
            .await
            .unwrap()
            .destination_balance,
        None
    );
    assert_eq!(
        balance_reads(&payment.delivery_chain.lock().unwrap().rpc_requests),
        [] as [Value; 0]
    );
    let explicit = || PublicSwapTracking {
        origin: &payment.origin,
        across: &across,
        orderbook: None,
        explicit: true,
        client: crate::OperationHttpClient::direct_for_tests(),
    };
    let progress = owner
        .track_public_swap(payment.operation, payment.id, explicit())
        .await
        .unwrap();
    // The unconfirmed next block already has zero; Check status still reads the confirmed fill.
    assert_eq!(progress.destination_balance, Some((amount, block)));
    {
        let mut chain = payment.delivery_chain.lock().unwrap();
        let reads = balance_reads(&chain.rpc_requests);
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0]["params"][0]["to"], json!(USDC));
        let input: Bytes = serde_json::from_value(
            reads[0]["params"][0]
                .get("input")
                .or_else(|| reads[0]["params"][0].get("data"))
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(
            PublicErc20::balanceOfCall::abi_decode(&input)
                .unwrap()
                .account,
            EXECUTOR
        );
        assert_eq!(
            reads[0]["params"][1],
            json!({"blockHash": block.hash, "requireCanonical": true})
        );
        for request in &chain.rpc_requests {
            if request["method"] == "eth_getBlockByNumber" {
                assert_eq!(request["params"][1], false);
            }
        }
        chain.head += 1;
    }
    let progress = owner
        .track_public_swap(payment.operation, payment.id, explicit())
        .await
        .unwrap();
    let at = payment
        .delivery_chain
        .lock()
        .unwrap()
        .block(block.number + 1);
    assert_eq!(progress.destination_balance, Some((U256::ZERO, at)));

    across_task.abort();
    payment.finish(owner).await;
}

// Across reports the deposit filled, but no endpoint of the destination chain serves
// whole-block receipts. The delivery is private, so Across's word is not recorded: the swap
// stays tracked, and is delivered once the fill can be read.
#[tokio::test(start_paused = true)]
async fn a_public_swaps_fill_that_cant_be_read_is_not_recorded_on_the_word_of_across() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    payment.handed_off(&owner).await;

    let (block, transaction_hash) = payment.fill(payment.sell_amount, U256::from(1_000), true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    payment.delivery_chain.lock().unwrap().receipt_error = Some(-32601);
    assert!(
        owner
            .observe_public_swap_bridge(
                payment.operation,
                payment.id,
                &across,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    assert!(
        owner
            .check_public_swap_bridge(
                payment.operation,
                payment.id,
                &across,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    let unread = payment.saved();
    assert_eq!(unread.swap.observations().bridge_outcome, None);
    assert_eq!(unread.outcome, None);
    assert_eq!(payment.to_track(&owner), [payment.id]);

    payment.delivery_chain.lock().unwrap().receipt_error = None;
    assert_eq!(
        owner
            .observe_public_swap_bridge(
                payment.operation,
                payment.id,
                &across,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        Some(SwapBridgeOutcome::DeliveredVerified {
            block,
            transaction_hash,
            output_amount: U256::from(1_000),
            shielded: true,
        })
    );

    across_task.abort();
    payment.finish(owner).await;
}

// Across reports the deposit expired: the swap is refunding and the destination account's
// shield unfilled. The refund is then verified on the chain the Public account pays on, in the
// final receipts of the block holding the transaction Across names: the deposited amount of the
// deposit's token from the pinned pool to the Public account. A verified refund is returned
// once, ends the swap's tracking, and releases nothing of the destination account.
#[tokio::test(start_paused = true)]
async fn a_refunding_public_swaps_refund_to_its_public_account_is_verified_on_the_origin_chain() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    payment.handed_off(&owner).await;
    let refund = || {
        owner.observe_public_swap_refund(
            payment.operation,
            payment.id,
            &payment.origin,
            &across,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };

    // A swap that isn't refunding has no refund to look for.
    assert_eq!(refund().await.unwrap(), None);
    *reply.lock().unwrap() = across_deposit("expired", None, None);
    assert_eq!(
        owner
            .observe_public_swap_bridge(
                payment.operation,
                payment.id,
                &across,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        Some(SwapBridgeOutcome::Refunding)
    );
    let refunding = payment.saved();
    assert_eq!(refunding.outcome, Some(SwapDestinationOutcome::Unfilled));
    assert_eq!(refunding.swap.observations().bridge_refund, None);
    // Across names no refund transaction yet.
    assert_eq!(refund().await.unwrap(), None);
    assert_eq!(payment.to_track(&owner), [payment.id]);

    // A transfer to another payee, and one of less than was deposited, are not the refund.
    let mut refunds = Vec::new();
    for (payee, value, verified) in [
        (Address::repeat_byte(0x99), payment.sell_amount, false),
        (payment.source, payment.sell_amount - U256::ONE, false),
        (payment.source, payment.sell_amount, true),
    ] {
        let number = payment.include(
            payment.spoke_pool,
            vec![(
                DESTINATION_TOKEN,
                Transfer {
                    from: payment.spoke_pool,
                    to: payee,
                    value,
                }
                .encode_log_data(),
            )],
        );
        payment.mine();
        let (block, refund_tx) = {
            let origin = payment.chain.lock().unwrap();
            (
                origin.chain.block(number),
                origin.chain.transactions.last().unwrap().0,
            )
        };
        refunds.push(refund_tx);
        *reply.lock().unwrap() = across_deposit("expired", None, Some(refund_tx));
        let expected = verified.then_some(SwapObservation {
            block,
            transaction_hash: Some(refund_tx),
        });
        assert_eq!(refund().await.unwrap(), expected, "refund to {payee}");
        assert_eq!(
            payment.saved().swap.observations().bridge_refund,
            expected,
            "refund to {payee}"
        );
    }
    // The caller refreshes the Public account's balances when a refund is returned, once.
    assert_eq!(refund().await.unwrap(), None);
    assert_eq!(payment.to_track(&owner), [] as [SwapUseId; 0]);

    // The destination account's shield stays signed, and keeps the account from another swap.
    let refunded = payment.saved();
    assert_eq!(refunded.shields, [payment.shield]);
    assert_eq!(refunded.outcome, Some(SwapDestinationOutcome::Unfilled));
    assert_eq!(
        swap_account_refusal(
            &refunded.record,
            1,
            SwapAccountRole::Destination { token: USDC },
            SwapAccountUse::New,
            SwapAdmissionEvidence::Recorded,
        ),
        Some(SwapAccountRefusal::EarlierDeliveryUnresolved)
    );

    across_task.abort();
    payment.finish_after_lookups(owner, &refunds).await;
}

// Across can fill a deposit it reported expired. Routine polling stops at the refund, and an
// explicit status check asks again: the verified fill replaces the refund, and the destination
// account's shield payload is recorded as run.
#[tokio::test(start_paused = true)]
async fn an_explicit_check_replaces_a_public_swaps_refund_with_its_verified_fill() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    payment.handed_off(&owner).await;
    let observe = || {
        owner.observe_public_swap_bridge(
            payment.operation,
            payment.id,
            &across,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };

    *reply.lock().unwrap() = across_deposit("expired", None, None);
    assert_eq!(observe().await.unwrap(), Some(SwapBridgeOutcome::Refunding));
    assert_eq!(
        payment.saved().outcome,
        Some(SwapDestinationOutcome::Unfilled)
    );

    let output = U256::from(1_000);
    let (block, transaction_hash) = payment.fill(payment.sell_amount, output, true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    assert_eq!(observe().await.unwrap(), Some(SwapBridgeOutcome::Refunding));
    let delivered = SwapBridgeOutcome::DeliveredVerified {
        block,
        transaction_hash,
        output_amount: output,
        shielded: true,
    };
    assert_eq!(
        owner
            .check_public_swap_bridge(
                payment.operation,
                payment.id,
                &across,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        Some(delivered.clone())
    );
    let saved = payment.saved();
    assert_eq!(saved.swap.observations().bridge_outcome, Some(delivered));
    assert_eq!(
        saved.outcome,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );

    across_task.abort();
    payment.finish(owner).await;
}

// The wallet stops after a direct deposit was included. Only the destination chain's owner is
// loaded again: no owner of the chain the Public account pays on, and no wallet session. It
// lists the swap, and its tracking steps read the hand-off, the fill and the shield, after
// which the swap is finished and no longer listed.
#[tokio::test(start_paused = true)]
async fn a_public_swap_resumes_from_its_destination_chains_owner_alone() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    assert!(matches!(
        payment.deposit(&owner).await.unwrap(),
        PublicSwapTransactionOutcome::Included { .. }
    ));
    owner.shutdown().await;
    drop(owner);

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    assert_eq!(payment.to_track(&restarted), [payment.id]);

    // The deposit's block isn't final yet. Inspecting the preceding finalized block changes
    // only durable scan progress, so the full-record change flag is true without a hand-off.
    let before = payment.saved();
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        PublicSwapProgress {
            changed: true,
            ..NO_PROGRESS
        }
    );
    let scanned = payment.saved();
    assert_eq!(
        scanned.swap.transactions()[0].deposit_scan_from_block,
        Some(ORIGIN_HEAD + 1)
    );
    assert_eq!(scanned.swap.observations(), before.swap.observations());
    assert_eq!(scanned.shields, before.shields);
    // With no newly finalized block, another poll has no record change.
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        NO_PROGRESS
    );
    // Its hand-off is read once it is, while Across still reports the deposit pending.
    payment.mine();
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        PublicSwapProgress {
            changed: true,
            ..NO_PROGRESS
        }
    );
    assert!(payment.saved().swap.observations().bridge_handoff.is_some());
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        NO_PROGRESS
    );

    let (block, transaction_hash) = payment.fill(payment.sell_amount, U256::from(1_000), true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    let delivered = PublicSwapProgress {
        changed: true,
        refresh_public_balances: false,
        finished: true,
        permit_used_up: false,
        destination_balance: None,
    };
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        delivered
    );
    assert_eq!(
        payment.saved().outcome,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );
    assert_eq!(payment.to_track(&restarted), [] as [SwapUseId; 0]);

    across_task.abort();
    payment.finish(restarted).await;
}

// The wallet stops with a signed order whose submission is still pending. The destination
// chain's owner alone resends it, then reads its settlement, the hand-off in it and the fill.
#[tokio::test]
async fn a_public_order_resumes_from_its_destination_chains_owner_alone() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(true).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    // The orderbook accepts the order, but its answer is lost.
    assert!(payment.order(&owner, &orderbook).await.is_err());
    owner.shutdown().await;
    drop(owner);

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    assert_eq!(payment.to_track(&restarted), [payment.id]);
    // An order is tracked through its orderbook.
    assert!(payment.track(&restarted, &across, None).await.is_err());

    // The first step resends the recorded order, which the orderbook answers as a duplicate.
    assert_eq!(
        payment
            .track(&restarted, &across, Some(&orderbook))
            .await
            .unwrap(),
        PublicSwapProgress {
            changed: true,
            ..NO_PROGRESS
        }
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    let order = payment.saved().swap.order().unwrap().clone();
    assert_eq!(order.submission_status(), SwapSubmissionStatus::Accepted);

    let (input, output) = (U256::from(1_020), U256::from(1_009));
    payment.settle(order.uid(), input, Some(&payment.deposited(input, output)));
    payment.mine();
    let (block, transaction_hash) = payment.fill(input, output, true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    assert_eq!(
        payment
            .track(&restarted, &across, Some(&orderbook))
            .await
            .unwrap(),
        PublicSwapProgress {
            changed: true,
            refresh_public_balances: false,
            finished: true,
            permit_used_up: false,
            destination_balance: None,
        }
    );
    assert_eq!(
        payment.saved().outcome,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );
    assert_eq!(payment.to_track(&restarted), [] as [SwapUseId; 0]);
    // An accepted order isn't sent again.
    assert_eq!(requests.lock().unwrap().len(), 2);

    across_task.abort();
    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}
