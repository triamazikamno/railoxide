use super::*;

// The Public account withdraws proceeds its proxy holds: one transaction to the factory, which
// deploys the proxy, carrying a batch of its own that pays only the account. It is recorded
// before its broadcast, and the withdrawal counts once its block is final. Tracking continues
// until the final query at the hook batch's deadline rules out a deposit.
#[tokio::test(start_paused = true)]
async fn held_public_order_proceeds_are_withdrawn_to_the_public_account_through_the_factory() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let profile = payment.origin.public_swap_profile().unwrap();
    let (proxy, factory) = (payment.proxy(), profile.cow_shed_factory());
    let order = payment.saved().swap.order().unwrap().clone();

    // Nothing is withdrawn from a proxy that holds no proceeds of the swap.
    assert!(
        owner
            .review_public_swap_withdrawal(
                payment.operation,
                payment.id,
                &payment.origin,
                100,
                1,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    let paid = U256::from(1_020);
    payment.settle(uid, paid, None);
    payment.mine();
    assert!(
        payment
            .observe(&owner, &orderbook)
            .await
            .proxy_holds_proceeds()
    );

    // Stopping the swap ends its approvals, and leaves the proceeds the user's to withdraw.
    payment.store.stop_swap_use(payment.operation).unwrap();
    assert!(
        payment
            .saved()
            .record
            .swap_use(payment.id)
            .unwrap()
            .is_stopped()
    );
    let refused = payment.approve(&owner).await.unwrap_err();
    assert!(refused.to_string().contains("stopped"));

    // The hook's factory call rolled back, so the proxy has no code yet.
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.balance = Some(paid);
        origin.codeless.push(proxy);
        origin.receipt_logs = vec![(
            DESTINATION_TOKEN,
            Transfer {
                from: proxy,
                to: payment.source,
                value: paid,
            }
            .encode_log_data(),
        )];
    }
    // The math and proxy reads are the broker's, over the swap's own client. A first endpoint
    // that fails them is withdrawn at its third strike, so the review's proxy read skips it.
    let (failing, failing_requests, failing_task) =
        crate::rpc_broker::tests::spawn_recording_rpc_mock(Arc::new(|request: Value| {
            json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": {"code": -32005, "message": "limit exceeded"}
            })
        }))
        .await;
    let mut failing_first = payment.origin.clone();
    failing_first.rpc_route = crate::RpcChainRoute::new(
        DESTINATION_CHAIN,
        vec![
            failing,
            payment.origin.rpc_route.configured_endpoints()[0]
                .expose_url()
                .clone(),
        ],
    );
    let client = OperationHttpClient::for_tests(
        crate::rpc_broker::tests::tagged_reqwest_client("swap"),
        OperationNetworkIsolation::Dedicated,
    );
    for _ in 0..2 {
        assert!(
            owner
                .public_swap_orders_available(&failing_first, &client)
                .await
                .unwrap()
        );
    }
    let failing_review = owner
        .review_public_swap_withdrawal(
            payment.operation,
            payment.id,
            &failing_first,
            100,
            1,
            &client,
        )
        .await
        .unwrap();
    assert_eq!(
        (failing_review.amount, failing_review.proxy_deployed),
        (paid, false)
    );
    assert_eq!(
        failing_requests
            .lock()
            .unwrap()
            .iter()
            .map(|(route, request)| (route.clone(), request["method"].clone()))
            .collect::<Vec<_>>(),
        [
            (Some("swap".to_owned()), json!("eth_getCode")),
            (Some("swap".to_owned()), json!("eth_getCode")),
            // The balance read's strike withdraws it before the proxy read.
            (Some("swap".to_owned()), json!("eth_call")),
        ]
    );
    failing_task.abort();
    let review = owner
        .review_public_swap_withdrawal(
            payment.operation,
            payment.id,
            &payment.origin,
            100,
            1,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let gas_limit =
        PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS + payment.origin.gas.gas_limit_buffer;
    assert_eq!(
        (review.proxy, review.token, review.amount),
        (proxy, DESTINATION_TOKEN, paid)
    );
    assert_eq!(
        (review.proxy_deployed, review.gas_limit, review.max_gas_cost),
        (false, gas_limit, U256::from(gas_limit) * U256::from(100))
    );

    let PublicSwapTransactionOutcome::Included {
        block_number,
        transaction_hash,
    } = owner
        .submit_public_swap_withdrawal_with_signer(
            payment.operation,
            payment.id,
            &payment.origin,
            &payment.signer,
            REVIEWED_FEE,
            &review,
            false,
            crate::OperationHttpClient::direct_for_tests(),
            &mut |_: PublicActionProgressUpdate| {},
        )
        .await
        .unwrap()
    else {
        panic!("the withdrawal succeeds");
    };

    // The one transaction goes to the factory, and its batch is the account's own: the bought
    // token's transfer of the reviewed amount to the account, signed by it for its proxy.
    let broadcasts = payment.chain.lock().unwrap().broadcasts.clone();
    let [(hash, sent)] = broadcasts.as_slice() else {
        panic!("only the withdrawal was broadcast");
    };
    assert_eq!((*hash, sent.to()), (transaction_hash, Some(factory)));
    let call = COWShedFactory::executeHooksCall::abi_decode(sent.input()).unwrap();
    assert_eq!(call.user, payment.source);
    assert_eq!(
        decode_withdrawal_calls(&call.calls, payment.source).unwrap(),
        (DESTINATION_TOKEN, paid)
    );
    let batch = ExecuteHooks {
        calls: call.calls.clone(),
        nonce: call.nonce,
        deadline: call.deadline,
    };
    assert_eq!(
        alloy::primitives::Signature::from_raw(&call.signature)
            .unwrap()
            .recover_address_from_prehash(&execute_hooks_digest(&batch, DESTINATION_CHAIN, proxy))
            .unwrap(),
        payment.source
    );

    // It was recorded as a withdrawal from the account before it was broadcast, and the
    // order's own batch is as it was signed.
    {
        let at_broadcast = payment.at_broadcast.lock().unwrap();
        let [(_, at_withdrawal)] = at_broadcast.as_slice() else {
            panic!("one broadcast arrived");
        };
        assert!(matches!(
            at_withdrawal.transactions(),
            [transaction] if transaction.kind == PublicSwapTransactionKind::Withdrawal
                && transaction.hash == transaction_hash
                && transaction.transaction.from == Some(payment.source)
                && transaction.inclusion.is_none()
        ));
    }
    let sent_swap = payment.saved().swap;
    assert_ne!(call.nonce, order.batch().nonce());
    assert_eq!(sent_swap.order(), Some(&order));
    let inclusion = sent_swap.transactions()[0].inclusion.unwrap();
    assert!(inclusion.succeeded);
    assert!(!inclusion.finalized);
    assert_eq!(inclusion.observation.block.number, block_number);

    // Its block isn't final yet, so the proxy still counts as holding the proceeds.
    assert_eq!(sent_swap.observations().withdrawn, None);
    assert!(
        payment
            .observe(&owner, &orderbook)
            .await
            .proxy_holds_proceeds()
    );
    let competing = || {
        let id = SwapUseId::random().unwrap();
        let operation = ExecutorOperationId::random().unwrap();
        let mut approval = sent_swap.approval().clone();
        approval.sell_token = Address::repeat_byte(0x5f);
        approval.destination = SwapApprovedAccount {
            address: None,
            setup: true,
        };
        crate::vault::PublicSwapClaim {
            id,
            origin_chain: DESTINATION_CHAIN,
            source: payment.source,
            source_scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: TEST_WALLET_ID.to_owned(),
            },
            account: SwapAccountChoice::New(operation),
            delegate: payment
                .destination
                .accepted_executor_profile()
                .unwrap()
                .delegate(),
            destination_token: USDC,
            bridged_token: DESTINATION_TOKEN,
            order: true,
            approval,
            now: u64::from(order.batch().deadline()) + 1,
        }
    };
    assert!(matches!(
        payment.store.claim_public_swap(competing()),
        Err(ExecutorStoreError::PublicSwapBuysSameToken { swap, .. }) if swap == payment.id
    ));

    // The saved inclusion disappears, and the same signed transaction is included in a later
    // block. The old block becomes final without the transaction, so its receipt is only a hint.
    let re_included_at = {
        let mut origin = payment.chain.lock().unwrap();
        origin.chain.reorg(block_number);
        let number = block_number + 2;
        origin.chain.head = number;
        origin.chain.include(
            number,
            sent.clone(),
            payment.source,
            true,
            vec![(
                DESTINATION_TOKEN,
                Transfer {
                    from: proxy,
                    to: payment.source,
                    value: paid,
                }
                .encode_log_data(),
            )],
        );
        number
    };
    let reorganized = payment.observe(&owner, &orderbook).await;
    assert_eq!(reorganized.observations().withdrawn, None);
    assert!(reorganized.proxy_holds_proceeds());
    assert!(matches!(
        payment.store.claim_public_swap(competing()),
        Err(ExecutorStoreError::PublicSwapBuysSameToken { .. })
    ));
    payment.mine();
    let withdrawn = payment.observe(&owner, &orderbook).await;
    let confirmed = withdrawn.observations().withdrawn.unwrap();
    assert_eq!(
        (confirmed.block.number, confirmed.transaction_hash),
        (re_included_at, Some(transaction_hash))
    );
    assert_ne!(confirmed.block, inclusion.observation.block);
    assert_eq!(
        withdrawn.transactions()[0].inclusion,
        Some(PublicSwapInclusion {
            observation: confirmed,
            succeeded: true,
            finalized: true,
        })
    );
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::SwappedNotBridged)
    );
    assert!(!withdrawn.proxy_holds_proceeds());
    assert_eq!(withdrawn.observations().deposit_ruled_out, None);
    assert!(!withdrawn.is_finished(true, u64::from(payment.valid_to) + 1));
    assert_eq!(payment.to_track(&owner), [payment.id]);
    let withdrawal_queries = payment.log_queries();
    assert!(withdrawal_queries > 0);
    // Withdrawn proceeds are not withdrawn again.
    assert!(
        owner
            .review_public_swap_withdrawal(
                payment.operation,
                payment.id,
                &payment.origin,
                100,
                1,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );

    // The withdrawal doesn't rule out an earlier late run of the batch. Wait for the first
    // block past its deadline to become final before concluding that no deposit was made.
    {
        let mut origin = payment.chain.lock().unwrap();
        let number = origin.chain.head + 1;
        origin.chain.head = number;
        origin.late_from = Some((number, u64::from(payment.valid_to) + 1));
    }
    payment.observe(&owner, &orderbook).await;
    assert_eq!(payment.to_track(&owner), [payment.id]);
    assert_eq!(payment.log_queries(), withdrawal_queries);
    payment.mine();
    let finished = payment.observe(&owner, &orderbook).await;
    assert!(finished.observations().deposit_ruled_out.is_some());
    assert!(finished.is_finished(true, u64::from(payment.valid_to) + 1));
    assert_eq!(payment.to_track(&owner), [] as [SwapUseId; 0]);
    assert_eq!(payment.log_queries(), withdrawal_queries + 1);
    payment.store.claim_public_swap(competing()).unwrap();

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

/// Invalidate the swap's order from its Public account.
async fn invalidate(
    payment: &PublicPayment,
    owner: &ExecutorOwner,
    orderbook: &CowOrderbookClient,
) -> eyre::Result<PublicSwapTransactionOutcome> {
    owner
        .submit_public_swap_invalidation_with_signer(
            payment.operation,
            payment.id,
            &payment.origin,
            &payment.signer,
            REVIEWED_FEE,
            orderbook,
            INVALIDATION_GAS_LIMIT,
            crate::OperationHttpClient::direct_for_tests(),
            &mut |_: PublicActionProgressUpdate| {},
        )
        .await
}

// The invalidation succeeds on an order that already filled, so its inclusion decides nothing:
// the orderbook reports the trade, and the swap follows its settlement.
#[tokio::test(start_paused = true)]
async fn an_invalidation_included_after_a_fill_leaves_the_public_order_traded() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    // The order filled and its hook deposited, which the wallet hasn't read yet.
    let paid = U256::from(1_020);
    let block = payment.settle(uid, paid, Some(&payment.deposited(paid, U256::from(1_009))));
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));

    assert!(matches!(
        invalidate(&payment, &owner, &orderbook).await.unwrap(),
        PublicSwapTransactionOutcome::Included { .. }
    ));
    // The invalidation's block made the settlement's final, and the trade was read there.
    let observed = payment.saved().swap.observations();
    assert_eq!(observed.cancelled, None);
    assert_eq!(observed.traded.unwrap().block.number, block);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));

    // Later observations keep it so.
    payment.mine();
    assert_eq!(
        payment
            .observe(&owner, &orderbook)
            .await
            .observations()
            .cancelled,
        None
    );
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// An order invalidated after it filled reads as cancelled, and past its validTo also as
// expired. The orderbook doesn't report the trade at the first lookup but does when asked again
// before either is recorded, so neither is: the swap stays tracked past its batch's deadline,
// and the next observation reads the trade.
#[tokio::test(start_paused = true)]
async fn an_invalidated_fill_the_orderbook_reports_late_is_neither_cancelled_nor_expired() {
    for past_valid_to in [false, true] {
        let (payment, owner) = PublicPayment::start_order().await;
        let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
        let uid = payment.submitted(&owner, &orderbook).await;

        let paid = U256::from(1_020);
        let block = payment.settle(uid, paid, Some(&payment.deposited(paid, U256::from(1_009))));
        *payment.trade_block.lock().unwrap() = None;
        let PublicSwapTransactionOutcome::Included { block_number, .. } =
            invalidate(&payment, &owner, &orderbook).await.unwrap()
        else {
            panic!("the invalidation succeeds");
        };
        {
            let mut origin = payment.chain.lock().unwrap();
            origin.chain.invalidated.push((uid, block_number));
            if past_valid_to {
                origin.late_from = Some((block_number, u64::from(payment.valid_to) + 1));
            }
        }

        // The invalidation's block is final. The orderbook reports no trade at the first
        // lookup, and the trade from then on.
        payment.mine();
        payment.trade_answers.lock().unwrap().push_back(None);
        *payment.trade_block.lock().unwrap() = Some(block);
        let swap = payment.observe(&owner, &orderbook).await;
        let observed = swap.observations();
        assert_eq!(
            (observed.cancelled, observed.expired, observed.traded),
            (None, None, None)
        );
        let deadline = u64::from(swap.order().unwrap().batch().deadline());
        assert!(!swap.is_finished(false, deadline + 1));

        let traded = payment.observe(&owner, &orderbook).await.observations();
        assert_eq!(traded.traded.unwrap().block.number, block);
        assert_eq!((traded.cancelled, traded.expired), (None, None));
        assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));

        orderbook_task.abort();
        let _ = orderbook_task.await;
        payment.finish(owner).await;
    }
}

// An invalidation only cancels once the settlement holds it in final canonical state. A
// provisional receipt that is reorganized out cannot release same-token selling or prevent
// another invalidation. The hook batch stays signed, so same-token buying remains blocked.
#[tokio::test(start_paused = true)]
async fn an_invalidation_without_a_trade_cancels_the_public_order_and_keeps_its_batch() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let order = payment.saved().swap.order().unwrap().clone();

    // A stopped swap's open order is the one the user wants gone.
    payment.store.stop_swap_use(payment.operation).unwrap();
    assert!(
        payment
            .saved()
            .record
            .swap_use(payment.id)
            .unwrap()
            .is_stopped()
    );
    let PublicSwapTransactionOutcome::Included {
        block_number,
        transaction_hash,
    } = invalidate(&payment, &owner, &orderbook).await.unwrap()
    else {
        panic!("the invalidation succeeds");
    };

    let provisional = payment.saved().swap;
    assert!(provisional.transactions()[0].inclusion.is_some());
    assert_eq!(provisional.observations().cancelled, None);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));
    assert!(provisional.order_can_fill(unix_now()));

    let (competing_id, competing_operation) = (
        SwapUseId::random().unwrap(),
        ExecutorOperationId::random().unwrap(),
    );
    let competing = || {
        let mut request = claim(
            competing_id,
            competing_operation,
            DESTINATION_CHAIN,
            ORDER_SELL_TOKEN,
            USDC,
        );
        request.source = payment.source;
        request
    };
    assert!(matches!(
        owner.claim_public_swap(competing()).unwrap_err().downcast_ref(),
        Some(ExecutorStoreError::PublicSwapSellsSameToken { swap, .. }) if *swap == payment.id
    ));

    // The provisional invalidation leaves the canonical chain. A final block now still reads
    // the order fill as zero, and its receipt cannot cancel the order on its own.
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.chain.reorg(block_number);
        origin.chain.invalidated.clear();
    }
    payment.mine();
    assert_eq!(
        payment
            .observe(&owner, &orderbook)
            .await
            .observations()
            .cancelled,
        None
    );
    assert!(owner.claim_public_swap(competing()).is_err());

    let PublicSwapTransactionOutcome::Included {
        block_number: retry_block,
        ..
    } = invalidate(&payment, &owner, &orderbook).await.unwrap()
    else {
        panic!("the replacement invalidation succeeds");
    };
    payment
        .chain
        .lock()
        .unwrap()
        .chain
        .invalidated
        .push((uid, retry_block));
    assert_eq!(
        payment
            .observe(&owner, &orderbook)
            .await
            .observations()
            .cancelled,
        None
    );
    payment.mine();
    let cancelled = payment.observe(&owner, &orderbook).await;
    let at = cancelled.observations().cancelled.unwrap();
    assert_eq!((at.block.number, at.transaction_hash), (retry_block, None));
    assert_eq!(cancelled.observations().traded, None);
    // The admission rule reports the same time for another order that buys the token: the
    // batch's deadline plus one.
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::Cancelled {
            retry_at: u64::from(order.batch().deadline()) + 1,
        })
    );
    assert!(cancelled.batch_can_run(unix_now()));
    assert!(!cancelled.order_can_fill(unix_now()));

    // Nothing was sent to the proxy or the factory.
    let broadcasts = payment.chain.lock().unwrap().broadcasts.clone();
    let (hash, sent) = &broadcasts[0];
    assert_eq!(broadcasts.len(), 2);
    assert_eq!(
        (*hash, sent.to(), &sent.input()[..]),
        (
            transaction_hash,
            Some(payment.settlement()),
            &invalidate_order_calldata(&uid)[..]
        )
    );
    assert!(matches!(
        cancelled.transactions(),
        [first, second] if first.kind == PublicSwapTransactionKind::Invalidation
            && first.hash == transaction_hash
            && second.kind == PublicSwapTransactionKind::Invalidation
    ));
    // The recorded batch has the nonce, deadline and calldata it was signed with.
    assert_eq!(cancelled.order().unwrap().batch(), order.batch());
    // Loading a finalized state observation preserves cancellation, and same-token selling
    // is admitted again even though the old batch may still buy its bridged token.
    assert_eq!(payment.saved().swap.observations().cancelled, Some(at));
    owner.claim_public_swap(competing()).unwrap();

    // A cancelled order is not invalidated again.
    assert!(invalidate(&payment, &owner, &orderbook).await.is_err());
    assert_eq!(payment.chain.lock().unwrap().broadcasts.len(), 2);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The order's state from hand-set observations: a hand-off wins over everything, then a
// withdrawal, then proceeds the proxy holds, and a cancellation wins over an expiry. An order
// past its `validTo` stays open until a final block shows it expired. Beside each state is
// whether tracking is finished with the swap once the batch's deadline has passed.
#[tokio::test]
async fn a_public_orders_state_follows_the_precedence_of_its_observations() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    // A swap that hasn't signed an order has no order state.
    assert_eq!(payment.state(), None);
    payment.submitted(&owner, &orderbook).await;

    let valid_to = u64::from(payment.valid_to);
    let at = SwapObservation {
        block: BlockNumHash::new(30, B256::repeat_byte(30)),
        transaction_hash: None,
    };
    let amount = U256::from(1_020);
    let expired = PublicSwapObservations {
        expired: Some(at),
        ..PublicSwapObservations::default()
    };
    let unsettled = PublicSwapObservations {
        cancelled: Some(at),
        ..expired.clone()
    };
    let held = PublicSwapObservations {
        traded: Some(at),
        held_by_proxy: Some(PublicSwapProxyHolding {
            observation: at,
            amount,
        }),
        ..unsettled.clone()
    };
    let ruled_out = PublicSwapObservations {
        deposit_ruled_out: Some(at),
        ..held.clone()
    };
    let withdrawn = PublicSwapObservations {
        withdrawn: Some(at),
        ..held.clone()
    };
    let withdrawn_without_deposit = PublicSwapObservations {
        deposit_ruled_out: Some(at),
        ..withdrawn.clone()
    };
    let bridged = PublicSwapObservations {
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: at,
            deposit_id: Some(U256::from(DEPOSIT_ID)),
        }),
        ..withdrawn.clone()
    };
    for (observations, expected, finished) in [
        (
            PublicSwapObservations::default(),
            PublicSwapOrderState::Open,
            false,
        ),
        (expired, PublicSwapOrderState::Expired, true),
        (
            unsettled,
            PublicSwapOrderState::Cancelled {
                retry_at: valid_to + 1,
            },
            true,
        ),
        (
            held,
            PublicSwapOrderState::HeldByProxy {
                amount,
                batch_live: true,
            },
            false,
        ),
        (
            ruled_out,
            PublicSwapOrderState::HeldByProxy {
                amount,
                batch_live: false,
            },
            false,
        ),
        (withdrawn, PublicSwapOrderState::SwappedNotBridged, false),
        (
            withdrawn_without_deposit,
            PublicSwapOrderState::SwappedNotBridged,
            true,
        ),
        // The bridge's outcome isn't recorded yet.
        (bridged, PublicSwapOrderState::Bridged, false),
    ] {
        payment
            .store
            .record_public_swap_observations(payment.operation, payment.id, observations.clone())
            .unwrap();
        let swap = payment.saved().swap;
        assert_eq!(
            public_swap_order_state(&swap),
            Some(expected),
            "{observations:?}"
        );
        assert_eq!(
            swap.is_finished(false, valid_to + 1),
            finished,
            "{observations:?}"
        );
        // An order that ended without a trade is tracked until its batch can no longer run.
        assert!(!swap.is_finished(false, valid_to) || observations.withdrawn.is_some());
    }
    // Admission already counts the open order as unable to fill once its `validTo` passes.
    payment
        .store
        .record_public_swap_observations(
            payment.operation,
            payment.id,
            PublicSwapObservations::default(),
        )
        .unwrap();
    assert!(!payment.saved().swap.order_can_fill(valid_to + 1));

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The settlement's receipt holds the order's trade and its hook's deposit, but no transfer to
// the proxy. The trade is the pinned settlement's, so it is recorded with what it bought as
// held by the proxy, and the deposit is left for the query at the batch's deadline.
#[tokio::test]
async fn a_public_orders_trade_without_its_payout_is_recorded_as_held_by_the_proxy() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    let paid = U256::from(1_020);
    let deposit = payment.deposited(paid, U256::from(1_009)).encode_log_data();
    let block = payment.include(
        payment.settlement(),
        vec![payment.trade(uid, paid), (payment.spoke_pool, deposit)],
    );
    *payment.trade_block.lock().unwrap() = Some(block);
    payment.mine();

    let observed = payment.observe(&owner, &orderbook).await.observations();
    let traded = observed.traded.unwrap();
    assert_eq!(traded.block.number, block);
    assert_eq!(
        observed.held_by_proxy,
        Some(PublicSwapProxyHolding {
            observation: traded,
            amount: paid,
        })
    );
    assert_eq!(observed.bridge_handoff, None);
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::HeldByProxy {
            amount: paid,
            batch_live: true,
        })
    );

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The wallet stops between a withdrawal's broadcast and its receipt, so its inclusion is never
// recorded. A new owner finds it by the bought token's transfers from the proxy to the Public
// account, verifies it in its block's receipts, and records the inclusion with the withdrawal.
#[tokio::test]
async fn a_withdrawal_whose_receipt_was_never_seen_is_found_by_its_transfer() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let proxy = payment.proxy();

    let paid = U256::from(1_020);
    payment.settle(uid, paid, None);
    payment.mine();
    payment.observe(&owner, &orderbook).await;
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.balance = Some(paid);
        origin.receipt_logs = vec![(
            DESTINATION_TOKEN,
            Transfer {
                from: proxy,
                to: payment.source,
                value: paid,
            }
            .encode_log_data(),
        )];
    }
    let review = owner
        .review_public_swap_withdrawal(
            payment.operation,
            payment.id,
            &payment.origin,
            100,
            1,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let mut progress = |_: PublicActionProgressUpdate| {};
    tokio::select! {
        outcome = owner.submit_public_swap_withdrawal_with_signer(
            payment.operation,
            payment.id,
            &payment.origin,
            &payment.signer,
            REVIEWED_FEE,
            &review,
            false, crate::OperationHttpClient::direct_for_tests(),
            &mut progress,
        ) => {
            panic!("the wallet stops before the receipt, but the withdrawal returned {outcome:?}");
        }
        () = payment.broadcast.notified() => {}
    }
    owner.shutdown().await;
    drop(owner);
    let included_at = payment.chain.lock().unwrap().chain.head;
    let interrupted = payment.saved().swap;
    let [withdrawal] = interrupted.transactions() else {
        panic!("the withdrawal was handed off");
    };
    assert_eq!(
        (withdrawal.kind, withdrawal.inclusion),
        (PublicSwapTransactionKind::Withdrawal, None)
    );
    let hash = withdrawal.hash;

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    // The withdrawal's block isn't final yet.
    assert!(
        payment
            .observe(&restarted, &orderbook)
            .await
            .proxy_holds_proceeds()
    );
    payment.mine();
    let found = payment.observe(&restarted, &orderbook).await;
    let withdrawn = found.observations().withdrawn.unwrap();
    assert_eq!(
        (withdrawn.block.number, withdrawn.transaction_hash),
        (included_at, Some(hash))
    );
    assert_eq!(
        found.transactions()[0].inclusion,
        Some(PublicSwapInclusion {
            observation: withdrawn,
            succeeded: true,
            finalized: true,
        })
    );
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::SwappedNotBridged)
    );

    // Every log query asked for the token's transfers from the proxy to the Public account,
    // and for nothing that names the transaction.
    {
        let chain = payment.chain.lock().unwrap();
        let queries: Vec<_> = chain
            .requests
            .iter()
            .filter(|request| request["method"] == "eth_getLogs")
            .map(|request| &request["params"][0])
            .collect();
        assert_ne!(queries, [] as [&Value; 0]);
        for filter in queries {
            let tokens: Vec<Address> = match &filter["address"] {
                Value::Array(_) => serde_json::from_value(filter["address"].clone()).unwrap(),
                token => vec![serde_json::from_value(token.clone()).unwrap()],
            };
            assert_eq!(tokens, [DESTINATION_TOKEN]);
            let topics: Vec<Option<B256>> =
                serde_json::from_value(filter["topics"].clone()).unwrap();
            assert_eq!(
                topics[..3],
                [
                    Some(Transfer::SIGNATURE_HASH),
                    Some(proxy.into_word()),
                    Some(payment.source.into_word()),
                ]
            );
            assert!(topics[3..].iter().all(Option::is_none));
            assert!(!filter.to_string().contains(&alloy::hex::encode(hash)));
        }
    }

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}

// The wallet stops between an invalidation's broadcast and its receipt. A new owner reads the
// settlement's fill of the order at a final block: the invalidation set it to the maximum, and
// the orderbook reports no trade, so the swap is cancelled at that block.
#[tokio::test]
async fn an_invalidation_whose_receipt_was_never_seen_cancels_from_the_settlements_state() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    tokio::select! {
        outcome = invalidate(&payment, &owner, &orderbook) => {
            panic!("the wallet stops before the receipt, but the invalidation returned {outcome:?}");
        }
        () = payment.broadcast.notified() => {}
    }
    owner.shutdown().await;
    drop(owner);
    // The settlement holds the order invalidated from the block that included the transaction.
    let invalidated_at = {
        let mut origin = payment.chain.lock().unwrap();
        let number = origin.chain.head;
        origin.chain.invalidated.push((uid, number));
        number
    };
    let interrupted = payment.saved().swap;
    let [invalidation] = interrupted.transactions() else {
        panic!("the invalidation was handed off");
    };
    assert_eq!(
        (invalidation.kind, invalidation.inclusion),
        (PublicSwapTransactionKind::Invalidation, None)
    );

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    // The invalidation's block isn't final yet.
    assert_eq!(
        payment
            .observe(&restarted, &orderbook)
            .await
            .observations()
            .cancelled,
        None
    );
    payment.mine();
    let cancelled = payment
        .observe(&restarted, &orderbook)
        .await
        .observations()
        .cancelled
        .unwrap();
    assert_eq!(
        (cancelled.block.number, cancelled.transaction_hash),
        (invalidated_at, None)
    );
    assert!(matches!(
        payment.state(),
        Some(PublicSwapOrderState::Cancelled { .. })
    ));

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}
