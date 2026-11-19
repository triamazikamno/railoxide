use super::*;

// An Across order pays the executor. Its post-hook deposits exactly the buy amount for the
// approved destination minimum on the terms of a quote fetched while signing, and shields any
// surplus. A quote below that minimum, or with a fill deadline the SpokePool could refuse,
// stops signing.
#[tokio::test]
async fn across_order_deposits_the_approved_terms_in_its_post_hook() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let profile = chain.accepted_executor_profile().unwrap();
    let swap_profile = chain.swap_profile().unwrap();
    let spoke_pool = chain.bridge_profile().unwrap().spoke_pool();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let receiver = Address::repeat_byte(0x93);
    let delivery = BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver,
        destination_token: POLYGON_WETH,
        surplus: BridgeSurplus::Reshield,
        private: None,
    };
    let fixture = BridgeOrderFixture::new(
        &owner,
        &store,
        &view,
        profile,
        &swap_profile,
        SwapDelivery::Bridge(delivery),
    )
    .await;
    let executor = fixture.executor;

    // The quote's output and fill deadline, in seconds after the request.
    let quoted = Arc::new(Mutex::new((U256::from(999), 3 * 60 * 60)));
    let answer = quoted.clone();
    let (across_url, across_requests, across_task) = spawn_bridge_stub(move |_| {
        let (output, fill_after) = *answer.lock().unwrap();
        across_fee_quote(spoke_pool, output, fill_after)
    })
    .await;
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook_with_lost_response(
        db.clone(),
        view.clone(),
        fixture.operation,
        swap_profile.settlement(),
        true,
    )
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        orderbook_url,
        1,
    )
    .unwrap();
    let mut clients = owner.swap_bridge_clients(&orderbook).unwrap();
    clients.across = crate::bridge::AcrossClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        across_url,
    )
    .unwrap();
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let destination = crate::bridge::BridgeDestination {
        destination_token: POLYGON_WETH,
        intermediate: WETH,
        symbol: "WETH".to_owned(),
        same_asset: true,
        near: None,
    };
    let route = crate::SwapBridgeRoute {
        clients: &clients,
        destination: crate::SwapBridgeDestination::Chain {
            destination: &destination,
            chain: chains.get(137).unwrap(),
        },
    };

    // A Bridge review keeps the profile's validity even when another is requested, since its
    // bridge leg is quoted for that window.
    let (quote_url, quotes, quote_task) = spawn_quote_stub(
        json!({
            "quote": {
                "sellToken": USDC, "buyToken": WETH, "sellAmount": "997500",
                "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0",
                "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": "1000000000000",
                "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }),
        None,
    )
    .await;
    let window = swap_profile.valid_to_window().as_secs();
    let quoted_from = unix_now();
    let reviewed = owner
        .review_swap(SwapReviewRequest {
            plan: fixture.review.plan().clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(30),
            orderbook: &CowOrderbookClient::new(
                OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
                quote_url,
                1,
            )
            .unwrap(),
            anchor_cache: None,
            token_registry: &crate::settings::EffectiveTokenRegistry {
                tokens: std::collections::BTreeMap::new(),
            },
            bridge: Some(route),
        })
        .await
        .unwrap();
    let quoted_until = unix_now();
    quote_task.abort();
    assert_eq!(reviewed.valid_for(), swap_profile.valid_to_window());
    let quote_valid_to = quotes.lock().unwrap()[0]["validTo"].as_u64().unwrap();
    assert!((quoted_from + window..=quoted_until + window).contains(&quote_valid_to));

    // Another gas share prices the held quote again and asks only Across, for the new buy
    // amount. The orderbook stub is gone, so a `CoW` request would fail.
    *quoted.lock().unwrap() = (U256::from(1_234), 3 * 60 * 60);
    let requoted = owner
        .requote_swap_bridge(
            &reviewed,
            crate::cow::GAS_SHARE_TIGHT_BPS,
            route,
            None,
            &crate::settings::EffectiveTokenRegistry {
                tokens: std::collections::BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    *quoted.lock().unwrap() = (U256::from(999), 3 * 60 * 60);
    let requoted_amount = requoted.suggested_private_minimum();
    assert_ne!(requoted_amount, reviewed.suggested_private_minimum());
    let (path, _) = across_requests.lock().unwrap().last().cloned().unwrap();
    assert!(
        path.contains(&format!("amount={requoted_amount}")),
        "{path}"
    );
    assert_eq!(requoted.gas_share_bps(), crate::cow::GAS_SHARE_TIGHT_BPS);
    assert_eq!(
        requoted.bridge().unwrap().destination_minimum,
        U256::from(1_234)
    );
    assert_eq!(
        (requoted.quote(), requoted.valid_for()),
        (reviewed.quote(), reviewed.valid_for())
    );
    assert_eq!(quotes.lock().unwrap().len(), 1);

    assert_eq!(
        fixture.issue(&owner, &orderbook, route).await.unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(999),
        })
    );
    fixture.assert_unsigned(&store, &submissions);

    // Base has neither a Railgun deployment nor a pinned pool, and still takes a public
    // delivery: its receiver passes the check, so Across is asked for the quote. The zero
    // address is refused there too.
    let swap_use = crate::vault::SwapUseId::first(fixture.operation);
    let approved = fixture.record(&store).swap_approval().unwrap().clone();
    let delegated = fixture.review.plan().swap_executor().delegated().unwrap();
    let base_route = crate::SwapBridgeRoute {
        destination: crate::SwapBridgeDestination::Chain {
            destination: &destination,
            chain: chains.get(8453).unwrap(),
        },
        ..route
    };
    let to_base = |receiver| {
        let base = BridgeOrderFixture::planned(
            &view,
            &swap_profile,
            delegated,
            executor,
            SwapDelivery::Bridge(BridgeDelivery {
                destination_chain: 8453,
                receiver,
                ..delivery
            }),
        );
        let approval = base
            .review
            .approval(base.review.suggested_private_minimum(), true)
            .unwrap();
        owner
            .record_swap_approval(fixture.operation, swap_use, approval)
            .unwrap();
        base
    };
    assert_eq!(
        to_base(receiver)
            .issue(&owner, &orderbook, base_route)
            .await
            .unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(999),
        })
    );
    assert_eq!(
        to_base(Address::ZERO)
            .issue(&owner, &orderbook, base_route)
            .await
            .unwrap_err()
            .downcast_ref::<BridgeReceiverRejection>(),
        Some(&BridgeReceiverRejection::ZeroAddress)
    );
    owner
        .record_swap_approval(fixture.operation, swap_use, approved)
        .unwrap();
    fixture.assert_unsigned(&store, &submissions);
    // A deadline that leaves relayers too little time after the order expires.
    *quoted.lock().unwrap() = (U256::from(1_005), 60);
    let error = fixture.issue(&owner, &orderbook, route).await.unwrap_err();
    assert!(
        error.to_string().contains("Across's quote can't be used"),
        "{error:#}"
    );
    fixture.assert_unsigned(&store, &submissions);

    // The approval allows gas of about 3% of the quote. The requote after setup finds gas 1%
    // higher and an Across output of 999 for its own deposit. Raising the deposit to deliver
    // 1,000 again costs less than a fifth of the approved allowed gas, so no review is needed.
    let plan = fixture.review.plan().clone();
    let gas_price_wei = 100_000_000_000_000_000 / u128::from(plan.hook_gas_estimate());
    let reviewed = BridgeOrderFixture::review_at(
        plan.clone(),
        &swap_profile,
        delivery.provider,
        gas_price_wei,
        1_000,
    );
    let approval = reviewed
        .approval(reviewed.suggested_private_minimum(), true)
        .unwrap();
    let requote = BridgeOrderFixture::review_at(
        plan,
        &swap_profile,
        delivery.provider,
        gas_price_wei + gas_price_wei / 100,
        999,
    );
    let buy_amount = requote.approved_order_minimum(&approval).unwrap();
    assert!(buy_amount > approval.bounds.private_minimum);

    // The order is persisted with the quote's terms and sent. Its response is lost, and the
    // resubmission rebuilds the identical order.
    *quoted.lock().unwrap() = (U256::from(1_005), 3 * 60 * 60);
    let signed_from = unix_now();
    assert!(
        fixture
            .issue_with(&owner, &orderbook, route, &requote, buy_amount, None)
            .await
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let signed_until = unix_now();
    let (path, _) = across_requests.lock().unwrap().last().cloned().unwrap();
    assert!(path.contains(&format!("amount={buy_amount}")), "{path}");
    let saved = fixture.record(&store).swap().unwrap().orders()[0].clone();
    let Some(BridgeOrderTerms::Across(terms)) = saved.bridge().cloned() else {
        panic!("an Across order keeps its deposit terms");
    };
    assert_eq!(
        terms,
        AcrossOrderTerms {
            spoke_pool,
            input_token: WETH,
            output_token: POLYGON_WETH,
            input_amount: buy_amount,
            // The approved minimum, not the quote's higher output.
            output_amount: U256::from(1_000),
            exclusive_relayer: Address::repeat_byte(0x77),
            exclusivity_parameter: 3,
            ..terms
        }
    );
    assert_eq!(saved.bounds().destination_minimum, Some(U256::from(1_000)));
    assert_eq!(saved.bounds().buy_amount, buy_amount);
    assert_eq!(
        owner
            .resubmit_swap_order(fixture.operation, &orderbook)
            .await
            .unwrap(),
        SwapOrderOutcome::Submitted { uid: saved.uid() }
    );
    let submitted = submissions.lock().unwrap().clone();
    let [(persisted, body), (_, resent)] = submitted.as_slice() else {
        panic!("one initial request and one resubmission");
    };
    assert!(*persisted, "the terms are durable before the order request");
    assert_eq!(body, resent);
    assert_eq!(submitted_order(body).receiver, executor);
    // The Bridge order is valid for the profile's window after signing.
    assert!(
        (signed_from + window..=signed_until + window)
            .contains(&u64::from(submitted_order(body).validTo))
    );

    // The post-hook guards, approves and deposits the buy amount, then shields the surplus.
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    let post_hook = RelayAdapt7702::multicallCall::abi_decode(&hooks.post[0].call_data).unwrap();
    let calls = post_hook._calls;
    assert_eq!(
        calls.iter().map(|call| call.to).collect::<Vec<_>>(),
        [executor, WETH, spoke_pool, executor]
    );
    let guard = transferCall::abi_decode(&calls[0].data).unwrap();
    assert_eq!(guard._transfers[0].value, buy_amount);
    let approval = approveCall::abi_decode(&calls[1].data).unwrap();
    assert_eq!(
        (approval.spender, approval.amount),
        (spoke_pool, buy_amount)
    );
    let deposit =
        broadcaster_core::contracts::across::SpokePool::depositV3Call::abi_decode(&calls[2].data)
            .unwrap();
    assert_eq!((deposit.depositor, deposit.recipient), (executor, receiver));
    assert_eq!(
        (
            deposit.inputToken,
            deposit.outputToken,
            deposit.destinationChainId
        ),
        (WETH, POLYGON_WETH, U256::from(137))
    );
    // The raised deposit still pays exactly the approved destination minimum.
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (buy_amount, U256::from(1_000))
    );
    assert_eq!(
        (
            deposit.quoteTimestamp,
            deposit.fillDeadline,
            deposit.exclusiveRelayer,
            deposit.exclusivityParameter
        ),
        (
            terms.quote_timestamp,
            terms.fill_deadline,
            terms.exclusive_relayer,
            terms.exclusivity_parameter
        )
    );
    assert!(deposit.message.is_empty());
    let shield =
        broadcaster_core::contracts::railgun::shieldCall::abi_decode(&calls[3].data).unwrap();
    assert_eq!(shield._shieldRequests[0].preimage.token.tokenAddress, WETH);

    // The order expires with its post-hook still signed at the nonce after the retry's
    // pre-hook, under its own minimum of 1,000. An ordinary retry discloses no continuing
    // minimum and signs without accepting one.
    let observed = fixture.record(&store).nonce_observation().unwrap();
    let expired = SwapOrderObservations {
        pre_hook_dead: Some(SwapPreHookDeath {
            cause: SwapPreHookDeathCause::Expired,
            observation: SwapObservation {
                block: observed.block(),
                transaction_hash: None,
            },
        }),
        ..SwapOrderObservations::default()
    };
    store
        .record_swap_observations(fixture.operation, saved.uid(), expired)
        .unwrap();
    // The fixture expires the order immediately; a real expiry also advances the deadline.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    *quoted.lock().unwrap() = (U256::from(1_200), 3 * 60 * 60);
    let (quote_url, _, quote_task) = spawn_quote_stub(
        json!({
            "quote": {
                "sellToken": USDC, "buyToken": WETH, "sellAmount": "997500",
                "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0",
                "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": "1000000000000",
                "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }),
        None,
    )
    .await;
    let tokens = crate::settings::EffectiveTokenRegistry {
        tokens: std::collections::BTreeMap::new(),
    };
    let retry = owner
        .review_swap(SwapReviewRequest {
            plan: fixture.review.plan().clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(30),
            orderbook: &CowOrderbookClient::new(
                OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
                quote_url,
                1,
            )
            .unwrap(),
            anchor_cache: None,
            token_registry: &tokens,
            bridge: Some(route),
        })
        .await
        .unwrap();
    quote_task.abort();
    let retry_minimum = retry.bridge().unwrap().destination_minimum;
    assert_eq!(retry_minimum, U256::from(1_200));
    assert_eq!(retry.continuing_minimum(), None);
    let output_pois = OutputPois::default();
    let authorization = password();
    let anchors = crate::TokenAnchorRateCache::new();
    let SwapOrderOutcome::Submitted { uid: retried } = owner
        .issue_swap_order(crate::SwapOrderSigning {
            review: &retry,
            swap_use,
            private_minimum: retry.suggested_private_minimum(),
            price_acknowledged: true,
            transactions: vec![fixture.transaction.clone()],
            inputs: std::slice::from_ref(&fixture.input),
            change_output_pois: Vec::new(),
            output_pois: &output_pois,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &anchors,
            token_registry: &tokens,
            bridge: Some(route),
            destination_minimum: Some(retry_minimum),
            destination: None,
            continuing_minimum: None,
        })
        .await
        .unwrap()
    else {
        panic!("the retry is submitted without accepting a continuing minimum");
    };
    let record = fixture.record(&store);
    let [_, retried_order] = record.swap().unwrap().orders() else {
        panic!("the retry is the second attempt");
    };
    assert_eq!(retried_order.uid(), retried);
    // The retry keeps its own higher minimum.
    assert_eq!(
        retried_order.bounds().destination_minimum,
        Some(retry_minimum)
    );

    // A speed-up of the open retry proposes 1,100 on the destination: above the 1,000 the
    // expired attempt's post-hook can still deliver, and below the retry's own 1,200.
    let plan = speed_up_plan(
        &store,
        &swap_profile,
        delegated,
        std::slice::from_ref(&fixture.input),
        retried,
    )
    .unwrap();
    *quoted.lock().unwrap() = (U256::from(1_100), 3 * 60 * 60);
    let (quote_url, _, quote_task) = spawn_quote_stub(usdc_weth_quote(), None).await;
    let speed_up = owner
        .review_swap(SwapReviewRequest {
            plan,
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_LOOSE_BPS,
            valid_for: Duration::from_mins(30),
            orderbook: &CowOrderbookClient::new(
                OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
                quote_url,
                1,
            )
            .unwrap(),
            anchor_cache: None,
            token_registry: &tokens,
            bridge: Some(route),
        })
        .await
        .unwrap();
    quote_task.abort();
    let speed_up_minimum = speed_up.bridge().unwrap().destination_minimum;
    assert_eq!(speed_up_minimum, U256::from(1_100));
    // The review discloses the expired attempt's 1,000 as a still possible outcome.
    assert_eq!(speed_up.continuing_minimum(), Some(U256::from(1_000)));
    // What qualifies the speed-up is its `CoW` buy amount, lower than the open retry's.
    let speed_up_buy = speed_up.suggested_private_minimum();
    assert!(speed_up_buy < retried_order.bounds().buy_amount);
    // Neither no acceptance nor a stale one, of a higher continuing minimum than the one that
    // now applies, signs anything.
    for (accepted, approved) in [(None, 0), (Some(U256::from(1_001)), 1_001)] {
        assert_eq!(
            sign_order(
                &owner,
                &orderbook,
                &speed_up,
                speed_up_buy,
                &fixture.transaction,
                &fixture.input,
                Some((route, speed_up_minimum, accepted)),
            )
            .await
            .unwrap(),
            SwapOrderOutcome::ReviewRequired(SwapReviewChange::ContinuingApproval {
                approved: U256::from(approved),
                current: U256::from(1_000),
            })
        );
        assert_eq!(fixture.record(&store), record);
        assert_eq!(submissions.lock().unwrap().len(), 3);
    }
    // With the 1,000 accepted, that lower minimum doesn't block the speed-up.
    let SwapOrderOutcome::Submitted { uid: sped_up } = sign_order(
        &owner,
        &orderbook,
        &speed_up,
        speed_up_buy,
        &fixture.transaction,
        &fixture.input,
        Some((route, speed_up_minimum, Some(U256::from(1_000)))),
    )
    .await
    .unwrap() else {
        panic!("the speed-up is submitted once the continuing minimum is accepted");
    };
    let record = fixture.record(&store);
    let [_, open, replacement] = record.swap().unwrap().orders() else {
        panic!("the speed-up is the third attempt");
    };
    assert_eq!(
        (replacement.uid(), replacement.invalidates()),
        (sped_up, Some(retried))
    );
    assert_eq!(
        replacement.bounds().destination_minimum,
        Some(speed_up_minimum)
    );
    assert!(replacement.bounds().buy_amount < open.bounds().buy_amount);

    across_task.abort();
    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// A speed-up signs a second order beside the open one. Both pre-hooks share the open order's
// nonce and the recorded proof and approve the same sell amount, the new pre-hook invalidates
// the open order, and the new order asks for less. A review that asks for no less signs
// nothing, and neither does one whose open order ended, was replaced or lost its nonce after
// planning.
#[tokio::test]
async fn a_speed_up_signs_a_second_order_at_the_open_orders_nonce() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let profile = chain.accepted_executor_profile().unwrap();
    let swap_profile = chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let executor = owner
        .prepare_swap_setup(
            operation,
            broadcaster(profile.delegate()),
            setup_approval(USDC, WETH, SwapDelivery::Reshield),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .context()
        .executor;
    // The setup wins nonce 0, so the first pre-hook signs at 1.
    let delegated = delegate_setup(&store, operation, profile);
    let record_of = || {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };

    let amount = U256::from(1_000_000);
    let note = |amount: U256, position: u64, random: [u8; 16]| {
        Utxo::new(
            broadcaster_core::notes::Note::new_change(
                view.scan_keys().master_public_key,
                USDC,
                amount,
                random,
            ),
            0,
            position,
            UtxoSource {
                tx_hash: B256::ZERO,
                block_number: 0,
                block_timestamp: 0,
            },
            UtxoCommitmentKind::Shield,
        )
    };
    let input = note(amount, 0, [7; 16]);
    // A larger note of the sell token, which the first order's proof doesn't spend.
    let other = note(amount * U256::from(2), 1, [8; 16]);
    let transaction = Transaction {
        proof: SnarkProof::default(),
        merkleRoot: B256::ZERO,
        nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
        commitments: vec![B256::ZERO],
        boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
        unshieldPreimage: CommitmentPreimage::empty(),
    };
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
    };
    let SwapAmountPlan::Fits(first_plan) = crate::plan_swap_inputs(
        &builder,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        &SwapAmountRequest {
            sell_token: USDC,
            buy_token: WETH,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        swap_profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("one note fits one order");
    };
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let quote: CowQuote = serde_json::from_value(usdc_weth_quote()).unwrap();
    // `plan`'s review of the same quote at `gas_share_bps`, valid for 30 minutes.
    let review_of = |plan: crate::SwapInputPlan, gas_share_bps: u16| {
        crate::price_swap_review(
            plan,
            quote.clone(),
            SwapPrice::Unverified,
            U256::from(25),
            U256::from(25),
            50,
            gas_share_bps,
            Duration::from_mins(30),
            1_000_000_000,
            U256::ZERO,
            isolation,
        )
        .unwrap()
    };
    let orderbook_client = |url: url::Url| {
        CowOrderbookClient::new(
            OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
            url,
            1,
        )
        .unwrap()
    };

    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook(
        db.clone(),
        view.clone(),
        operation,
        swap_profile.settlement(),
    )
    .await;
    let orderbook = orderbook_client(orderbook_url);
    let first_review = review_of(first_plan, GAS_SHARE_BALANCED_BPS);
    let first_minimum = first_review.suggested_private_minimum();
    let SwapOrderOutcome::Submitted { uid: first } = sign_order(
        &owner,
        &orderbook,
        &first_review,
        first_minimum,
        &transaction,
        &input,
        None,
    )
    .await
    .unwrap() else {
        panic!("the first order is submitted");
    };
    let record = record_of();
    let open = record.swap().unwrap().orders()[0].clone();
    assert_eq!(open.uid(), first);

    // Planning funds the speed-up from the recorded proof alone. Without that proof's note
    // nothing is planned, and another note offered beside it is never selected.
    assert!(
        speed_up_plan(
            &store,
            &swap_profile,
            delegated,
            std::slice::from_ref(&other),
            first
        )
        .is_err()
    );
    let plan = speed_up_plan(
        &store,
        &swap_profile,
        delegated,
        &[other.clone(), input.clone()],
        first,
    )
    .unwrap();
    assert_eq!(
        plan.speed_up(),
        Some(crate::vault::SwapSpeedUp {
            swap_use: crate::vault::SwapUseId::first(operation),
            order: first,
            pre_hook_nonce: open.pre_hook().nonce(),
        })
    );
    assert_eq!(plan.invalidates(), Some(first));
    let request = plan.proof_request(&swap_profile).unwrap();
    assert!(
        builder
            .preview_mixed_private_action_plan(std::slice::from_ref(&input), &request)
            .is_ok()
    );
    assert!(matches!(
        builder.preview_mixed_private_action_plan(std::slice::from_ref(&other), &request),
        Err(railgun_wallet::tx::BuildError::PinnedInputUnavailable { .. })
    ));

    // A larger gas share lowers the minimum, and with it the order's buy amount.
    let review = review_of(plan, GAS_SHARE_LOOSE_BPS);
    let minimum = review.suggested_private_minimum();
    assert!(review.buy_amount_for(minimum).unwrap() < open.bounds().buy_amount);

    // An order that asks for the open order's buy amount or more relaxes nothing. Nothing is
    // signed, persisted or sent for it.
    for not_lower in [first_minimum, first_minimum + U256::from(1_000)] {
        let outcome = sign_order(
            &owner,
            &orderbook,
            &review,
            not_lower,
            &transaction,
            &input,
            None,
        )
        .await
        .unwrap();
        let SwapOrderOutcome::ReviewRequired(SwapReviewChange::SpeedUpBuyAmount {
            open: limit,
            current,
        }) = outcome
        else {
            panic!("an order that asks for no less isn't a speed-up: {outcome:?}");
        };
        assert!(limit == open.bounds().buy_amount && current >= limit);
        assert_eq!(record_of(), record);
        assert_eq!(submissions.lock().unwrap().len(), 1);
    }

    // The open order expires or trades after planning. The reviewed speed-up signs nothing.
    let seen = SwapObservation {
        block: record.nonce_observation().unwrap().block(),
        transaction_hash: None,
    };
    for ended in [
        SwapOrderObservations {
            expired: Some(seen),
            ..SwapOrderObservations::default()
        },
        SwapOrderObservations {
            traded: Some(seen),
            ..SwapOrderObservations::default()
        },
    ] {
        store
            .record_swap_observations(operation, first, ended)
            .unwrap();
        let stale = sign_order(
            &owner,
            &orderbook,
            &review,
            minimum,
            &transaction,
            &input,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            stale.downcast_ref::<SwapAccountRefusal>(),
            Some(&SwapAccountRefusal::SpeedUpOrderChanged),
            "{stale:#}"
        );
        assert_eq!(record_of().swap().unwrap().orders().len(), 1);
        assert_eq!(submissions.lock().unwrap().len(), 1);
    }
    store
        .record_swap_observations(operation, first, SwapOrderObservations::default())
        .unwrap();
    assert_eq!(record_of(), record);

    // The speed-up is persisted and sent, and the response is lost.
    let (lost_url, resent, lost_task) = spawn_orderbook_with_lost_response(
        db.clone(),
        view.clone(),
        operation,
        swap_profile.settlement(),
        true,
    )
    .await;
    let lost = orderbook_client(lost_url);
    let signed_from = unix_now();
    assert!(
        sign_order(&owner, &lost, &review, minimum, &transaction, &input, None)
            .await
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let signed_until = unix_now();
    let sped_up = record_of();
    let [kept, second] = sped_up.swap().unwrap().orders() else {
        panic!("the speed-up is the second order");
    };
    // The open order, the setup's approval and the proof are as they were.
    assert_eq!(kept, &open);
    assert_eq!(sped_up.swap_approval(), record.swap_approval());
    assert_eq!(
        sped_up.swap().unwrap().proof(),
        record.swap().unwrap().proof()
    );
    // The new order is an attempt of the same use at the same pre-hook nonce. It names the
    // order it invalidates, sells the same amount and asks for less.
    assert_eq!(second.use_id(), kept.use_id());
    assert_eq!(second.pre_hook().nonce(), kept.pre_hook().nonce());
    assert_eq!(second.invalidates(), Some(first));
    assert_eq!(second.bounds().sell_amount, kept.bounds().sell_amount);
    assert!(second.bounds().buy_amount < kept.bounds().buy_amount);
    // Its expiry is the unchanged 30 minutes after its own signing.
    assert_eq!(second.bounds().valid_for_secs, kept.bounds().valid_for_secs);
    assert!((signed_from + 1_800..=signed_until + 1_800).contains(&u64::from(second.valid_to())));

    // The unknown outcome resends the recorded order. No third order is signed.
    assert_eq!(second.submission_status(), SwapSubmissionStatus::Pending);
    assert_eq!(
        owner.resubmit_swap_order(operation, &lost).await.unwrap(),
        SwapOrderOutcome::Submitted { uid: second.uid() }
    );
    let sent = resent.lock().unwrap().clone();
    let [(persisted, body), (_, again)] = sent.as_slice() else {
        panic!("one initial request and one resubmission");
    };
    assert!(
        *persisted,
        "the speed-up is durable before its order request"
    );
    assert_eq!(body, again);
    assert_eq!(record_of().swap().unwrap().orders().len(), 2);
    let order = submitted_order(body);
    assert_eq!(
        (order.sellAmount, order.buyAmount),
        (kept.bounds().sell_amount, second.bounds().buy_amount)
    );
    // The new pre-hook signs at the shared nonce, invalidates the open order on the
    // settlement and approves exactly the unchanged sell amount.
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    let pre_hook = RelayAdapt7702::executeCall::abi_decode(&hooks.pre[0].call_data).unwrap();
    assert_eq!(pre_hook._nonce, kept.pre_hook().nonce());
    let calls = pre_hook._actionData.calls;
    assert!(calls.iter().any(|call| {
        call.to == swap_profile.settlement()
            && GPv2Settlement::invalidateOrderCall::abi_decode(&call.data)
                .is_ok_and(|invalidated| invalidated.orderUid[..] == first.0[..])
    }));
    let approval = approveCall::abi_decode(&calls.last().unwrap().data).unwrap();
    assert_eq!(
        (approval.spender, approval.amount),
        (swap_profile.vault_relayer(), kept.bounds().sell_amount)
    );

    // The review that named the first order can't sign again: it is no longer the latest.
    let stale = sign_order(
        &owner,
        &orderbook,
        &review,
        minimum,
        &transaction,
        &input,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        stale.downcast_ref::<SwapAccountRefusal>(),
        Some(&SwapAccountRefusal::SpeedUpOrderChanged),
        "{stale:#}"
    );

    // Once the first order's expiry is final, the second can be planned for. The account's
    // nonce then passes the shared one before signing, and nothing is signed at it.
    let second = second.uid();
    store
        .record_swap_observations(
            operation,
            first,
            SwapOrderObservations {
                expired: Some(seen),
                ..SwapOrderObservations::default()
            },
        )
        .unwrap();
    let plan = speed_up_plan(
        &store,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        second,
    )
    .unwrap();
    let review = review_of(plan, GAS_SHARE_LOOSE_BPS);
    store
        .record_account_read(
            operation,
            ExecutorNonceObservation::new(
                BlockNumHash::new(13, B256::repeat_byte(13)),
                U256::from(2),
            ),
        )
        .unwrap();
    let stale = sign_order(
        &owner,
        &orderbook,
        &review,
        review.suggested_private_minimum(),
        &transaction,
        &input,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        stale.downcast_ref::<SwapAccountRefusal>(),
        Some(&SwapAccountRefusal::SpeedUpNonceUsed),
        "{stale:#}"
    );
    assert_eq!(record_of().swap().unwrap().orders().len(), 2);
    assert_eq!(submissions.lock().unwrap().len(), 1);
    assert_eq!(resent.lock().unwrap().len(), 2);

    orderbook_task.abort();
    lost_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
