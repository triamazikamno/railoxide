use super::*;

// The reads that decide a token's permit travel in the allowance read's one request. A
// confirmed domain is kept for the session, so the next plan adds only the account's nonce. A
// permit the claimed order wasn't priced with returns to review. Reads that failed decide
// nothing and are asked again, while a token that answered without a permit is asked no more.
#[tokio::test]
async fn permit_reads_travel_with_the_allowance_read_and_are_kept_for_the_session() {
    let (payment, owner) = PublicPayment::start_on(ORDER_SELL_TOKEN, U256::ZERO, true, false).await;
    payment.chain.lock().unwrap().permit = Some((permit_domain(), PERMIT_NONCE));
    let relayer = payment
        .origin
        .public_swap_profile()
        .unwrap()
        .vault_relayer();
    let mut aggregating = payment.origin.clone();
    aggregating.rpc_route = aggregating
        .rpc_route
        .clone()
        .with_multicall(Address::repeat_byte(0xca));
    let plan = async |origin: &EffectiveChainConfig, token: Address| {
        owner
            .public_swap_approval_plan(
                origin,
                payment.source,
                token,
                relayer,
                payment.sell_amount,
                true,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
    };

    assert!(plan(&aggregating, ORDER_SELL_TOKEN).await.signs_permit());
    assert_eq!(token_reads_served(&payment), [6]);
    assert!(plan(&aggregating, ORDER_SELL_TOKEN).await.signs_permit());
    assert_eq!(token_reads_served(&payment), [2]);

    // The order was approved while its allowance covered the sale, without a pre-hook.
    let approval = payment.saved().swap.approval().clone();
    assert_eq!(
        payment.approve(&owner).await.unwrap(),
        PublicSwapApprovalsOutcome::ReviewRequired {
            transactions: false
        }
    );
    assert_eq!(
        payment.chain.lock().unwrap().broadcasts,
        [] as [(B256, TxEnvelope); 0]
    );
    assert_eq!(payment.saved().swap.approval(), &approval);
    token_reads_served(&payment);

    // Without Multicall3 each read is a request of its own, and one that fails is no answer.
    let other = Address::repeat_byte(0x5f);
    payment.chain.lock().unwrap().permit_reads_fail = true;
    for _ in 0..2 {
        assert_eq!(
            plan(&payment.origin, other).await.transactions(),
            [payment.sell_amount]
        );
        assert_eq!(token_reads_served(&payment), [1; 6]);
    }
    {
        let mut chain = payment.chain.lock().unwrap();
        chain.permit_reads_fail = false;
        chain.permit = None;
    }
    for reads in [6, 1] {
        assert_eq!(
            plan(&payment.origin, other).await.transactions(),
            [payment.sell_amount]
        );
        assert_eq!(token_reads_served(&payment), vec![1; reads]);
    }

    payment.finish(owner).await;
}

// An order whose sold token has a permit sends no approval. The account signs the permit for
// exactly the sold amount to the vault relayer until the order's `validTo`, under its current
// nonce, and the order carries it as a pre-hook on the token. The signature recovers the
// account over the digest the token computes, encoded here by hand. The signed call ran on
// the token by itself before the order left, and the permit is recorded with the order.
#[tokio::test]
async fn a_public_order_carries_a_signed_permit_in_place_of_an_approval() {
    let (payment, owner) = PublicPayment::start_permit_order().await;
    let relayer = payment
        .origin
        .public_swap_profile()
        .unwrap()
        .vault_relayer();
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;

    assert_eq!(
        payment.approve(&owner).await.unwrap(),
        PublicSwapApprovalsOutcome::Ready
    );
    let uid = payment.submitted(&owner, &orderbook).await;

    let served = requests.lock().unwrap().clone();
    let [(_, body)] = served.as_slice() else {
        panic!("one order request was sent");
    };
    let app_data: AppData = serde_json::from_str(body["appData"].as_str().unwrap()).unwrap();
    let [hook] = app_data.metadata.hooks.pre.as_slice() else {
        panic!("the order has one pre-hook");
    };
    assert_eq!(
        (hook.target, hook.gas_limit),
        (ORDER_SELL_TOKEN, PERMIT_HOOK_GAS_LIMIT)
    );
    let call = IERC20Permit::permitCall::abi_decode(&hook.call_data).unwrap();
    let deadline = U256::from(payment.valid_to);
    assert_eq!(
        (call.owner, call.spender, call.value, call.deadline),
        (payment.source, relayer, payment.sell_amount, deadline)
    );

    let separator = keccak256(
        (
            keccak256(
                "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
            ),
            keccak256("Token"),
            keccak256("2"),
            U256::from(DESTINATION_CHAIN),
            ORDER_SELL_TOKEN,
        )
            .abi_encode(),
    );
    let permit = keccak256(
        (
            keccak256(
                "Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)",
            ),
            payment.source,
            relayer,
            payment.sell_amount,
            PERMIT_NONCE,
            deadline,
        )
            .abi_encode(),
    );
    let digest = keccak256([&[0x19, 0x01][..], &separator[..], &permit[..]].concat());
    assert!(matches!(call.v, 27 | 28));
    assert_eq!(
        Signature::from_scalars_and_parity(call.r, call.s, call.v == 28)
            .recover_address_from_prehash(&digest)
            .unwrap(),
        payment.source
    );

    let order = payment.saved().swap.order().unwrap().clone();
    let recorded = order.permit().unwrap();
    assert_eq!(
        (
            order.uid(),
            recorded.nonce(),
            recorded.deadline(),
            recorded.value()
        ),
        (uid, PERMIT_NONCE, payment.valid_to, payment.sell_amount)
    );
    {
        let chain = payment.chain.lock().unwrap();
        assert_eq!(chain.simulated, std::slice::from_ref(&hook.call_data));
        assert_eq!(chain.broadcasts, [] as [(B256, TxEnvelope); 0]);
    }

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// A signed permit that reverts on the token stops the order: no path is recorded and the
// orderbook hears nothing. The account's nonce is still the signed one, so the token's permit
// isn't the standard one, and the session plans transactions for it. The approval covers
// none, so they return to review unsent.
#[tokio::test]
async fn a_permit_that_reverts_on_its_token_stops_the_order_before_it_is_recorded() {
    let (payment, owner) = PublicPayment::start_permit_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;
    payment.chain.lock().unwrap().permit_reverts = true;
    let approval = payment.saved().swap.approval().clone();

    let error = payment.order(&owner, &orderbook).await.unwrap_err();
    assert!(error.to_string().contains("permit couldn't be used"));
    assert_eq!(payment.chain.lock().unwrap().simulated.len(), 1);
    assert!(payment.saved().swap.path().is_none());
    assert!(requests.lock().unwrap().is_empty());

    assert_eq!(
        payment.approve(&owner).await.unwrap(),
        PublicSwapApprovalsOutcome::ReviewRequired { transactions: true }
    );
    assert_eq!(
        payment.chain.lock().unwrap().broadcasts,
        [] as [(B256, TxEnvelope); 0]
    );
    assert_eq!(payment.saved().swap.approval(), &approval);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// A token whose domain changed after the session confirmed it, here by a new version, reverts
// the permit signed under the old one. The token is asked again and the domain it now reports
// replaces the old, so that attempt fails and the next signs under the new domain.
#[tokio::test]
async fn a_permit_under_a_domain_the_token_replaced_fails_once_then_signs_under_the_new_one() {
    let (payment, owner) = PublicPayment::start_permit_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;
    // The first plan confirms the domain the token reports then.
    assert_eq!(
        payment.approve(&owner).await.unwrap(),
        PublicSwapApprovalsOutcome::Ready
    );
    let replaced = Eip712Domain::new(
        Some("Token".into()),
        Some("3".into()),
        Some(U256::from(DESTINATION_CHAIN)),
        Some(ORDER_SELL_TOKEN),
        None,
    );
    {
        let mut chain = payment.chain.lock().unwrap();
        chain.permit = Some((replaced.clone(), PERMIT_NONCE));
        chain.permit_reverts = true;
    }

    let error = payment.order(&owner, &orderbook).await.unwrap_err();
    assert!(error.to_string().contains("permit couldn't be used"));
    assert!(payment.saved().swap.path().is_none());
    assert!(requests.lock().unwrap().is_empty());

    // The mock token checks no signature, so the next permit is let through and the domain
    // it was signed under is checked here.
    payment.chain.lock().unwrap().permit_reverts = false;
    payment.submitted(&owner, &orderbook).await;
    let simulated = payment.chain.lock().unwrap().simulated.clone();
    let [_, signed] = simulated.as_slice() else {
        panic!("each attempt called its signed permit");
    };
    let call = IERC20Permit::permitCall::abi_decode(signed).unwrap();
    let digest = Permit {
        owner: call.owner,
        spender: call.spender,
        value: call.value,
        nonce: PERMIT_NONCE,
        deadline: call.deadline,
    }
    .eip712_signing_hash(&replaced);
    assert_eq!(
        Signature::from_scalars_and_parity(call.r, call.s, call.v == 28)
            .recover_address_from_prehash(&digest)
            .unwrap(),
        payment.source
    );

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The approval's pre-hook gas limit is what authorizes a permit, so it follows the review's
// approval plan. A permit review corrected once the allowance covers the sale binds no
// pre-hook. When the allowance is gone again, the permit the plan then finds returns to
// review, and nothing is signed.
#[tokio::test]
async fn a_permit_review_corrected_to_a_covered_allowance_binds_no_permit_hook() {
    let (payment, owner) = PublicPayment::start_permit_order().await;
    let client = || {
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        )
    };
    let spoke_pool = payment.spoke_pool;
    let (across_url, _, across_task) =
        spawn_bridge_stub(move |_| across_fee_quote(spoke_pool, U256::from(1_000), 3 * 60 * 60))
            .await;
    let across = AcrossClient::new(client(), across_url).unwrap();
    let (orderbook_url, _, orderbook_task) = spawn_bridge_stub(move |_| {
        json!({
            "quote": {
                "sellToken": ORDER_SELL_TOKEN, "buyToken": DESTINATION_TOKEN,
                "sellAmount": "1010", "buyAmount": "1010", "validTo": 1,
                "feeAmount": "0", "gasAmount": "0", "gasPrice": "0",
                "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        })
        .to_string()
    })
    .await;
    let orderbook = CowOrderbookClient::new(client(), orderbook_url, DESTINATION_CHAIN).unwrap();
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
    let review = owner
        .review_public_swap(crate::PublicSwapReviewRequest {
            origin: &payment.origin,
            source: payment.source,
            sell: crate::bridge::PublicSellAsset::Erc20(ORDER_SELL_TOKEN),
            sell_amount: U256::from(1_010),
            destination: &destination,
            slippage_bps: 0,
            gas_share_bps: 0,
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
        .unwrap();
    let claimed = payment.saved().swap.approval().clone();
    let approval = |review: &crate::PublicSwapReview| {
        review
            .approval(
                claimed.destination,
                claimed.bounds.destination_setup_fee,
                true,
            )
            .unwrap()
    };
    assert!(review.approval_plan().signs_permit());
    assert_eq!(
        approval(&review).bounds.pre_hook_gas_limit,
        PERMIT_HOOK_GAS_LIMIT
    );

    payment.chain.lock().unwrap().allowance = U256::MAX;
    let corrected = owner
        .requote_public_swap_approvals(
            &review,
            &payment.origin,
            payment.source,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(!corrected.approval_plan().signs_permit());
    assert_eq!(corrected.approval_plan().transactions(), [] as [U256; 0]);
    let corrected = approval(&corrected);
    assert_eq!(corrected.bounds.pre_hook_gas_limit, 0);

    owner
        .reapprove_public_swap(payment.operation, payment.id, corrected)
        .unwrap();
    payment.chain.lock().unwrap().allowance = U256::ZERO;
    assert_eq!(
        payment.approve(&owner).await.unwrap(),
        PublicSwapApprovalsOutcome::ReviewRequired {
            transactions: false
        }
    );
    assert_eq!(payment.chain.lock().unwrap().simulated, [] as [Bytes; 0]);

    for task in [across_task, orderbook_task] {
        task.abort();
        let _ = task.await;
    }
    payment.finish(owner).await;
}

/// The signed permit in a record's JSON.
fn recorded_permit(value: &mut Value) -> Option<&mut Value> {
    match value {
        Value::Object(fields) => {
            if fields.contains_key("permit") {
                return fields.get_mut("permit");
            }
            fields.values_mut().find_map(recorded_permit)
        }
        Value::Array(items) => items.iter_mut().find_map(recorded_permit),
        _ => None,
    }
}

// The orderbook's answer to a permit order is lost and the wallet stops. A new owner resends
// the recorded order with the recorded permit as its pre-hook: the same body, and nothing is
// read from the token or signed. A record whose permit isn't the signed one rebuilds another
// order, which isn't sent.
#[tokio::test]
async fn a_restart_resends_a_permit_order_with_its_recorded_permit() {
    let (payment, owner) = PublicPayment::start_permit_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(true).await;
    assert!(payment.order(&owner, &orderbook).await.is_err());
    let interrupted = payment.saved();
    let order = interrupted.swap.order().unwrap().clone();
    assert!(order.permit().is_some());
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
    let mut altered = serde_json::to_value(&interrupted.record).unwrap();
    recorded_permit(&mut altered).unwrap()["value"] = json!(U256::ONE);
    let altered: ExecutorRecord = serde_json::from_value(altered).unwrap();
    payment
        .store
        .put_operation_fixture(payment.operation, &altered)
        .unwrap();
    let refused = restarted
        .resubmit_public_swap_order(payment.operation, payment.id, &payment.origin, &orderbook)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("reconstructed unchanged"));
    assert_eq!(requests.lock().unwrap().len(), 1);

    payment
        .store
        .put_operation_fixture(payment.operation, &interrupted.record)
        .unwrap();
    token_reads_served(&payment);
    let uid = restarted
        .resubmit_public_swap_order(payment.operation, payment.id, &payment.origin, &orderbook)
        .await
        .unwrap();
    assert_eq!(uid, order.uid());
    let served = requests.lock().unwrap().clone();
    let [(_, first), (at_resend, resent)] = served.as_slice() else {
        panic!("the order was sent twice");
    };
    assert_eq!(first.to_string(), resent.to_string());
    assert_eq!(at_resend.as_ref(), Some(&order));
    let app_data: AppData = serde_json::from_str(resent["appData"].as_str().unwrap()).unwrap();
    assert_eq!(app_data.metadata.hooks.pre.len(), 1);
    assert_eq!(token_reads_served(&payment), [] as [usize; 0]);
    assert_eq!(payment.chain.lock().unwrap().simulated.len(), 1);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}

// While a permit order is open, its tracking reads the account's permit nonce and allowance
// and the settlement's fill of the order. A permit someone submitted early leaves the
// allowance in place and is no warning. A nonce another signature used with the allowance
// short is one: the order stays open, tracked and exclusive, and nothing is written. The
// warning clears once the allowance covers the sale. A settlement uses the nonce and the
// allowance too, and its fill keeps the warning off before the record holds the trade final,
// also while the orderbook reports no trade yet.
#[tokio::test]
async fn a_used_up_permit_warns_while_its_order_stays_open_and_exclusive() {
    let (payment, owner) = PublicPayment::start_permit_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let (across, _reply, _lookups, across_task) = across_stub().await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let track = async |nonce: u64, allowance: U256| {
        {
            let mut chain = payment.chain.lock().unwrap();
            chain.permit = Some((permit_domain(), U256::from(nonce)));
            chain.allowance = allowance;
        }
        payment
            .track(&owner, &across, Some(&orderbook))
            .await
            .unwrap()
    };

    assert!(!track(4, U256::ZERO).await.permit_used_up);
    assert!(!track(5, payment.sell_amount).await.permit_used_up);

    let open = payment.saved().swap;
    let warned = track(6, U256::ZERO).await;
    assert_eq!(
        (warned.changed, warned.permit_used_up, warned.finished),
        (true, true, false)
    );
    assert!(owner.public_swap_permit_used_up(payment.id));
    assert_eq!(payment.saved().swap, open);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));
    assert_eq!(payment.to_track(&owner), [payment.id]);
    assert!(matches!(
        payment
            .store
            .public_swap_source_conflict(&PublicSwapSourceTerms {
                id: None,
                origin_chain: DESTINATION_CHAIN,
                source: payment.source,
                source_scope: PublicAccountScope::PrivateWallet {
                    wallet_uuid: TEST_WALLET_ID.to_owned(),
                },
                sell_token: ORDER_SELL_TOKEN,
                bridged_token: WETH,
                order: true,
                now: unix_now(),
            })
            .unwrap(),
        Some(ExecutorStoreError::PublicSwapSellsSameToken { .. })
    ));

    let covered = track(6, payment.sell_amount).await;
    assert_eq!((covered.changed, covered.permit_used_up), (true, false));
    assert!(!owner.public_swap_permit_used_up(payment.id));

    // The settlement's block isn't final yet, so the record holds no trade, and the orderbook
    // doesn't report it yet either. The settlement's fill of the order is what shows it.
    let (input, output) = (U256::from(1_020), U256::from(1_009));
    let block = payment.settle(uid, input, Some(&payment.deposited(input, output)));
    *payment.trade_block.lock().unwrap() = None;
    assert!(!track(7, U256::ZERO).await.permit_used_up);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));
    *payment.trade_block.lock().unwrap() = Some(block);
    payment.mine();
    assert!(!track(7, U256::ZERO).await.permit_used_up);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));

    across_task.abort();
    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}
