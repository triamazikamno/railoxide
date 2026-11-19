use super::*;

// The Public account signs its proxy's hook batch, then the order that carries it. Both are in
// the record before the orderbook request, which names the account as owner and the proxy as
// receiver. The batch's signature recovers the account over the digest cow-shed verifies, and
// the order's over the settlement's.
#[tokio::test]
async fn a_public_order_is_recorded_with_its_signed_batch_before_it_is_submitted() {
    let (payment, owner) = PublicPayment::start_order().await;
    let profile = payment.origin.public_swap_profile().unwrap();
    let settlement = profile.settlement();
    let (source, proxy) = (payment.source, payment.proxy());
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;

    // The batch's terms can be shown before anything is signed, for the nonce the caller drew.
    let shown = owner
        .public_swap_order_batch_terms(
            payment.operation,
            payment.id,
            &payment.origin,
            &payment.delivery,
            &payment.terms,
            payment.valid_to,
            payment.batch_nonce,
        )
        .unwrap();
    assert!(payment.saved().swap.path().is_none());

    let PublicSwapOrderOutcome::Submitted { uid } =
        payment.order(&owner, &orderbook).await.unwrap()
    else {
        panic!("the order is submitted");
    };
    let recorded = payment.saved().swap;
    let order = recorded.order().unwrap().clone();
    assert_eq!(recorded.bridge(), Some(&payment.terms));
    assert_eq!(
        (
            order.uid(),
            order.buy_token(),
            order.proxy(),
            order.valid_to(),
            order.submission_status(),
        ),
        (
            uid,
            DESTINATION_TOKEN,
            proxy,
            payment.valid_to,
            SwapSubmissionStatus::Accepted,
        )
    );

    // The request found the order recorded and still pending.
    let served = requests.lock().unwrap().clone();
    let [(at_request, body)] = served.as_slice() else {
        panic!("one order request was sent");
    };
    let at_request = at_request.as_ref().unwrap();
    assert_eq!(
        (at_request.uid(), at_request.submission_status()),
        (uid, SwapSubmissionStatus::Pending)
    );
    assert_eq!(at_request.batch(), order.batch());

    // The Public account owns a fill-or-kill sell order that pays its proxy.
    let sent = submitted_order(body);
    assert_eq!(
        body["from"].as_str().unwrap().parse::<Address>().unwrap(),
        source
    );
    assert_eq!(body["signingScheme"], "eip712");
    assert_eq!(body["quoteId"], ORDER_QUOTE_ID);
    assert_eq!(
        (sent.sellToken, sent.buyToken, sent.receiver),
        (ORDER_SELL_TOKEN, DESTINATION_TOKEN, proxy)
    );
    assert_eq!(
        (
            sent.sellAmount,
            sent.buyAmount,
            sent.validTo,
            sent.feeAmount
        ),
        (
            payment.sell_amount,
            U256::from(1_010),
            payment.valid_to,
            U256::ZERO
        )
    );
    assert_eq!(
        (sent.kind.as_str(), sent.partiallyFillable),
        (ORDER_KIND_SELL, false)
    );
    let digest = order_digest(&sent, DESTINATION_CHAIN, settlement);
    assert_eq!(order_uid(&sent, DESTINATION_CHAIN, settlement, source), uid);
    let signature: Bytes = serde_json::from_value(body["signature"].clone()).unwrap();
    let signature: [u8; 65] = signature[..].try_into().unwrap();
    assert_eq!(recover_order_signer(&signature, &digest).unwrap(), source);
    assert_eq!(order.submission().signature(), &signature);

    // The app data holds one hook: the batch, run through the factory for the account.
    let document = body["appData"].as_str().unwrap();
    assert_eq!(keccak256(document), sent.appData);
    let app_data: AppData = serde_json::from_str(document).unwrap();
    assert_eq!(
        app_data.metadata.hooks.pre,
        [] as [broadcaster_core::contracts::cow::AppDataHook; 0]
    );
    let [hook] = app_data.metadata.hooks.post.as_slice() else {
        panic!("the order has one post-hook");
    };
    assert_eq!(
        (hook.target, hook.gas_limit),
        (profile.cow_shed_factory(), ORDER_HOOK_GAS_LIMIT)
    );
    assert_eq!(&hook.call_data, order.batch().calldata());
    let call = COWShedFactory::executeHooksCall::abi_decode(&hook.call_data).unwrap();
    assert_eq!(
        (call.user, call.nonce, call.deadline),
        (source, order.batch().nonce(), U256::from(payment.valid_to))
    );
    assert_eq!(order.batch().deadline(), payment.valid_to);
    let batch = ExecuteHooks {
        calls: call.calls.clone(),
        nonce: call.nonce,
        deadline: call.deadline,
    };
    assert!(matches!(call.signature[64], 27 | 28));
    assert_eq!(
        alloy::primitives::Signature::from_raw(&call.signature)
            .unwrap()
            .recover_address_from_prehash(&execute_hooks_digest(&batch, DESTINATION_CHAIN, proxy))
            .unwrap(),
        source
    );

    // The batch deposits the proxy's balance for the approved terms and the signed delivery.
    let signed = decode_deposit_hook_calls(&call.calls).unwrap();
    let deposit = &signed.deposit;
    assert_eq!(
        (signed.weiroll, deposit.math, deposit.spoke_pool),
        (profile.weiroll(), profile.math(), payment.spoke_pool)
    );
    assert_eq!(
        (
            deposit.proxy,
            deposit.depositor,
            deposit.destination_chain_id
        ),
        (proxy, source, 1)
    );
    assert_eq!(
        (deposit.buy_amount, deposit.destination_min),
        (U256::from(1_010), U256::from(1_000))
    );
    assert_eq!(
        (deposit.input_token, deposit.output_token),
        (DESTINATION_TOKEN, USDC)
    );
    assert_eq!(deposit.delivery, payment.delivery);
    // The batch the orderbook received is the one whose terms were shown for that nonce.
    assert_eq!(call.nonce, payment.batch_nonce);
    assert_eq!(shown, public_swap_batch_terms(&batch, deposit));

    // Only the proxy's balance of the bought token was read, and the account sent nothing.
    {
        let chain = payment.chain.lock().unwrap();
        assert_eq!(chain.balance_reads, [(DESTINATION_TOKEN, proxy)]);
        assert_eq!(
            chain.allowance_reads,
            [] as [(Address, Address, Address); 0]
        );
        assert_eq!(chain.broadcasts, [] as [(B256, TxEnvelope); 0]);
    }

    // The same request again finds the accepted order and neither signs nor sends anything.
    assert_eq!(
        payment.order(&owner, &orderbook).await.unwrap(),
        PublicSwapOrderOutcome::Submitted { uid }
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(payment.saved().swap.order(), Some(&order));

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The orderbook accepts the order, but its answer is lost and the wallet stops. A new owner
// resends the recorded order: the same body with the same signatures, and nothing is signed.
// The delivery was signed for a buy amount raised within the approval's cushion, which the
// order and its batch carry and the resent order is rebuilt with.
#[tokio::test]
async fn a_restart_before_the_orderbook_answers_resends_the_recorded_order() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(true).await;
    let raised = AcrossOrderTerms {
        input_amount: U256::from(1_013),
        ..payment.terms
    };

    assert!(
        payment
            .order_for(&owner, &orderbook, &payment.delivery, &raised)
            .await
            .is_err()
    );
    let interrupted = payment.saved().swap.order().unwrap().clone();
    assert_eq!(
        interrupted.submission_status(),
        SwapSubmissionStatus::Pending
    );
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
    let uid = restarted
        .resubmit_public_swap_order(payment.operation, payment.id, &payment.origin, &orderbook)
        .await
        .unwrap();
    assert_eq!(uid, interrupted.uid());

    let served = requests.lock().unwrap().clone();
    let [(_, first), (at_resend, resent)] = served.as_slice() else {
        panic!("the order was sent twice");
    };
    assert_eq!(first.to_string(), resent.to_string());
    assert_eq!(at_resend.as_ref(), Some(&interrupted));
    // The order buys the raised amount, and its batch guards and scales by it for the approved
    // destination minimum.
    assert_eq!(submitted_order(resent).buyAmount, raised.input_amount);
    let call =
        COWShedFactory::executeHooksCall::abi_decode(interrupted.batch().calldata()).unwrap();
    let deposit = decode_deposit_hook_calls(&call.calls).unwrap().deposit;
    assert_eq!(
        (deposit.buy_amount, deposit.destination_min),
        (raised.input_amount, U256::from(1_000))
    );
    assert_eq!(payment.saved().swap.bridge(), Some(&raised));
    // The duplicate answer is the acceptance, and the record's batch and signature are the
    // ones signed before the restart.
    let accepted = payment.saved().swap.order().unwrap().clone();
    assert_eq!(accepted.submission_status(), SwapSubmissionStatus::Accepted);
    assert_eq!(
        (accepted.uid(), accepted.batch(), accepted.submission()),
        (
            interrupted.uid(),
            interrupted.batch(),
            interrupted.submission()
        )
    );
    // An accepted order is not sent again.
    assert_eq!(
        restarted
            .resubmit_public_swap_order(payment.operation, payment.id, &payment.origin, &orderbook)
            .await
            .unwrap(),
        uid
    );
    assert_eq!(requests.lock().unwrap().len(), 2);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}

// A balance the proxy already holds would be deposited with the order's proceeds, so nothing
// is signed while it is there, or while it can't be read.
#[tokio::test]
async fn a_public_order_is_not_signed_while_its_proxy_holds_the_bought_token() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;
    let proxy = payment.proxy();

    for balance in [Some(U256::from(5)), None] {
        payment.chain.lock().unwrap().balance = balance;
        assert_eq!(
            payment.order(&owner, &orderbook).await.unwrap(),
            PublicSwapOrderOutcome::ProxyHoldsBoughtToken { proxy, balance }
        );
    }
    let recorded = payment.saved().swap;
    assert!(recorded.path().is_none());
    assert!(recorded.bridge().is_none());
    assert!(requests.lock().unwrap().is_empty());

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// A delivery to another account than the approved one, or terms for an amount below the
// approved buy amount, beyond its cushion or below the approved destination minimum, are
// refused before the proxy is read and before anything is signed.
#[tokio::test]
async fn a_public_order_for_other_terms_than_approved_is_refused_unsigned() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;

    let other_account = AcrossPrivateDelivery {
        destination_executor: Address::repeat_byte(0x99),
        ..payment.delivery.clone()
    };
    let [lower_amount, beyond_cushion] = [1_009, 1_031].map(|amount| AcrossOrderTerms {
        input_amount: U256::from(amount),
        ..payment.terms
    });
    let short = AcrossOrderTerms {
        output_amount: U256::from(999),
        ..payment.terms
    };
    for (delivery, terms) in [
        (&other_account, &payment.terms),
        (&payment.delivery, &lower_amount),
        (&payment.delivery, &beyond_cushion),
        (&payment.delivery, &short),
    ] {
        let error = payment
            .order_for(&owner, &orderbook, delivery, terms)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("review the swap again"));
    }
    assert!(payment.saved().swap.path().is_none());
    assert_eq!(
        payment.chain.lock().unwrap().balance_reads,
        [] as [(Address, Address); 0]
    );
    assert!(requests.lock().unwrap().is_empty());

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// An order's post-hook calls the math contract. Where its address has no code, or other code,
// the hook would fail after the trade and strand the proceeds in the proxy, so an order is
// neither reviewed nor signed there. Once the pinned code is at the address the same order is
// placed.
#[tokio::test]
async fn an_order_is_refused_until_the_math_contract_is_deployed_on_its_chain() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;
    let (across, _, _, across_task) = across_stub().await;
    let registry = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let destination = crate::bridge::PublicBridgeDestination {
        destination: crate::bridge::BridgeDestination {
            destination_token: USDC,
            intermediate: DESTINATION_TOKEN,
            symbol: "USDC".into(),
            same_asset: true,
            near: None,
        },
        path: crate::bridge::PublicBridgePath::Order,
    };
    let refusal = crate::PublicSwapUnavailable::OrdersNotEnabled {
        chain_id: DESTINATION_CHAIN,
    };

    for code in [Bytes::new(), Bytes::from_static(&[0x60, 0x80])] {
        payment.chain.lock().unwrap().math_code = code;
        assert!(
            !owner
                .public_swap_orders_available(
                    &payment.origin,
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap()
        );
        let error = owner
            .review_public_swap(crate::PublicSwapReviewRequest {
                origin: &payment.origin,
                source: payment.source,
                sell: crate::bridge::PublicSellAsset::Erc20(ORDER_SELL_TOKEN),
                sell_amount: payment.sell_amount,
                destination: &destination,
                slippage_bps: 50,
                gas_share_bps: 5_000,
                on_shield_failure: BridgeShieldFailure::KeepOnDestination,
                orderbook: Some(&orderbook),
                across: &across,
                anchor_cache: None,
                token_registry: &registry,
                anchor_deviation_bps: 300,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 1,
                client: crate::OperationHttpClient::direct_for_tests(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref(), Some(&refusal));
        assert_eq!(
            error.to_string(),
            "Swaps aren't available from this network yet. Only tokens that bridge directly can be sent."
        );
        let error = payment.order(&owner, &orderbook).await.unwrap_err();
        assert_eq!(error.downcast_ref(), Some(&refusal));
    }
    // Nothing was read of the proxy, signed, recorded or sent.
    assert!(payment.saved().swap.path().is_none());
    assert_eq!(
        payment.chain.lock().unwrap().balance_reads,
        [] as [(Address, Address); 0]
    );
    assert!(requests.lock().unwrap().is_empty());

    payment.chain.lock().unwrap().math_code = swap_math_runtime_code();
    assert!(
        owner
            .public_swap_orders_available(
                &payment.origin,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap()
    );
    assert!(matches!(
        payment.order(&owner, &orderbook).await.unwrap(),
        PublicSwapOrderOutcome::Submitted { .. }
    ));
    assert_eq!(requests.lock().unwrap().len(), 1);

    // A chain without a Public swap profile takes no orders, whatever code it has.
    let mut unprofiled = payment.origin.clone();
    unprofiled.enabled = false;
    assert!(
        !owner
            .public_swap_orders_available(
                &unprofiled,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap()
    );

    for task in [orderbook_task, across_task] {
        task.abort();
        let _ = task.await;
    }
    payment.finish(owner).await;
}

// A direct deposit runs no hook, so it never reads the math contract and goes through on a
// chain that doesn't have it.
#[tokio::test(start_paused = true)]
async fn a_direct_deposit_needs_no_math_contract() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::ZERO).await;
    payment.chain.lock().unwrap().math_code = Bytes::new();

    payment.approve(&owner).await.unwrap();
    assert!(matches!(
        payment.deposit(&owner).await.unwrap(),
        PublicSwapTransactionOutcome::Included { .. }
    ));
    assert!(
        !payment
            .chain
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|request| {
                request["method"] == "eth_getCode"
                    && serde_json::from_value(request["params"][0].clone()).ok()
                        == Some(SWAP_MATH_ADDRESS)
            })
    );

    payment.finish(owner).await;
}

// A settlement that pays more than the order's buy amount and whose hook deposited it: once
// its block is final the trade, its amounts and the hand-off are recorded together, with the
// amounts the deposit's event carries.
#[tokio::test]
async fn a_settled_public_order_whose_hook_deposited_is_bridged() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));
    // Without a reported trade there is nothing to read.
    assert_eq!(
        payment.observe(&owner, &orderbook).await.observations(),
        PublicSwapObservations::default()
    );

    let (paid, output) = (U256::from(1_020), U256::from(1_009));
    let block = payment.settle(uid, paid, Some(&payment.deposited(paid, output)));
    // The settlement's block isn't final yet.
    assert_eq!(
        payment
            .observe(&owner, &orderbook)
            .await
            .observations()
            .traded,
        None
    );
    payment.mine();
    let observed = payment.observe(&owner, &orderbook).await.observations();

    let traded = observed.traded.unwrap();
    assert_eq!(traded.block.number, block);
    let amounts = observed.trade_amounts.unwrap();
    assert_eq!(
        (amounts.sell_amount, amounts.buy_amount),
        (payment.sell_amount, paid)
    );
    assert_eq!(
        observed.bridge_handoff,
        Some(SwapBridgeHandoff {
            observation: traded,
            deposit_id: Some(U256::from(DEPOSIT_ID)),
        })
    );
    // Both are above the signed minimums of 1,010 and 1,000.
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: paid,
            output_amount: output,
        })
    );
    assert_eq!(observed.held_by_proxy, None);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));
    assert_eq!(payment.log_queries(), 0);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// A deposit in the settlement's receipt that puts out less than the approved destination
// minimum is not the swap's hand-off: the proxy holds the proceeds.
#[tokio::test]
async fn a_deposit_below_the_approved_minimum_is_not_the_public_orders_handoff() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    let paid = U256::from(1_020);
    payment.settle(uid, paid, Some(&payment.deposited(paid, U256::from(999))));
    payment.mine();
    let observed = payment.observe(&owner, &orderbook).await.observations();
    assert_eq!(observed.bridge_handoff, None);
    assert_eq!(observed.deposited, None);
    assert_eq!(
        observed.held_by_proxy,
        Some(PublicSwapProxyHolding {
            observation: observed.traded.unwrap(),
            amount: paid,
        })
    );

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// A settlement whose hook didn't deposit leaves the payout with the proxy. The batch can still
// run until its deadline, so no deposit is looked for before the first block past it is final.
// Then one query for the Public account's deposits finds none, the deposit is ruled out, and
// nothing is asked again.
#[tokio::test]
async fn proceeds_a_public_orders_hook_left_are_held_until_the_batch_deadline_rules_a_deposit_out()
{
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    let paid = U256::from(1_020);
    let block = payment.settle(uid, paid, None);
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
    assert_eq!(
        (observed.bridge_handoff, observed.deposit_ruled_out),
        (None, None)
    );
    let live = PublicSwapOrderState::HeldByProxy {
        amount: paid,
        batch_live: true,
    };
    assert_eq!(payment.state(), Some(live));
    // More final blocks before the deadline change nothing and ask for no logs.
    payment.mine();
    payment.observe(&owner, &orderbook).await;
    assert_eq!(payment.state(), Some(live));
    assert_eq!(payment.log_queries(), 0);

    // The next block is the first past the deadline. Nothing is concluded until it is final.
    let deadline_block = {
        let mut origin = payment.chain.lock().unwrap();
        let number = origin.chain.head + 1;
        origin.chain.head = number;
        origin.late_from = Some((number, u64::from(payment.valid_to) + 1));
        number
    };
    payment.observe(&owner, &orderbook).await;
    assert_eq!(payment.state(), Some(live));
    assert_eq!(payment.log_queries(), 0);

    payment.mine();
    let ruled_out = payment
        .observe(&owner, &orderbook)
        .await
        .observations()
        .deposit_ruled_out
        .unwrap();
    assert_eq!(
        (ruled_out.block.number, ruled_out.transaction_hash),
        (deadline_block, None)
    );
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::HeldByProxy {
            amount: paid,
            batch_live: false,
        })
    );
    assert_eq!(payment.log_queries(), 1);

    // The query ran once: later calls ask the chain for no logs.
    payment.mine();
    payment.observe(&owner, &orderbook).await;
    assert_eq!(payment.log_queries(), 1);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// Someone runs the batch in a later transaction before its deadline. Once the first block past
// the deadline is final, one query for the Public account's deposits finds it and the swap is
// bridged. The proxy's balance is never read to decide that, whatever it still holds.
#[tokio::test]
async fn a_late_run_of_a_public_orders_batch_is_found_at_its_deadline() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    let paid = U256::from(1_020);
    payment.settle(uid, paid, None);
    payment.mine();
    let held = payment
        .observe(&owner, &orderbook)
        .await
        .observations()
        .held_by_proxy;
    assert!(held.is_some());

    // The late run deposits what the proxy held, and a stray unit stays behind.
    let factory = payment
        .origin
        .public_swap_profile()
        .unwrap()
        .cow_shed_factory();
    let output = U256::from(1_009);
    let deposit_block = payment.include(
        factory,
        vec![(
            payment.spoke_pool,
            payment.deposited(paid, output).encode_log_data(),
        )],
    );
    let balance_reads = {
        let mut origin = payment.chain.lock().unwrap();
        origin.balance = Some(U256::ONE);
        let number = origin.chain.head + 1;
        origin.chain.head = number + 1;
        origin.late_from = Some((number, u64::from(payment.valid_to) + 1));
        origin.balance_reads.len()
    };

    let observed = payment.observe(&owner, &orderbook).await.observations();
    let handoff = observed.bridge_handoff.unwrap();
    assert_eq!(handoff.deposit_id, Some(U256::from(DEPOSIT_ID)));
    assert_eq!(handoff.observation.block.number, deposit_block);
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: paid,
            output_amount: output,
        })
    );
    // The payout stays recorded as history, and no deposit was ruled out.
    assert_eq!(observed.held_by_proxy, held);
    assert_eq!(observed.deposit_ruled_out, None);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));
    assert_eq!(payment.log_queries(), 1);
    assert_eq!(
        payment.chain.lock().unwrap().balance_reads.len(),
        balance_reads,
        "the proxy's balance decides nothing"
    );

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}
