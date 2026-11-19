use super::*;

// A NEAR Intents order pays the deposit address of a 1Click quote requested while signing,
// which names the receiver and refunds to the executor, and has no post-hook. A quote that
// doesn't verify, or that can't deliver the approved minimum, stops signing.
#[tokio::test]
async fn near_intents_order_pays_a_verified_deposit_address() {
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
    let receiver = Address::repeat_byte(0x93);
    let delivery = BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 137,
        receiver,
        destination_token: Address::repeat_byte(0x94),
        surplus: BridgeSurplus::BridgedByProvider,
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
    let buy_amount = fixture.review.suggested_private_minimum();

    // 1Click signs the request it was sent. The test key stands in for 1Click's.
    let quote_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    let pinned = format!(
        "ed25519:{}",
        bs58::encode(quote_key.verifying_key().to_bytes()).into_string()
    );
    let deposit_address = Address::repeat_byte(0xd0);
    // The address the next quote creates.
    let deposit = Arc::new(Mutex::new(deposit_address));
    let paying = deposit.clone();
    // The quote's minimum output and the key that signs it.
    let quoted = Arc::new(Mutex::new((
        U256::from(1_000),
        ed25519_dalek::SigningKey::from_bytes(&[8; 32]),
    )));
    let answer = quoted.clone();
    let (near_url, near_requests, near_task) = spawn_bridge_stub(move |request| {
        let (minimum, key) = answer.lock().unwrap().clone();
        let response = json!({
            "quote": {
                "amountIn": request["amount"],
                "amountInFormatted": "1",
                "amountInUsd": "1",
                "minAmountIn": request["amount"],
                "amountOut": (minimum + U256::from(10)).to_string(),
                "amountOutFormatted": "1",
                "amountOutUsd": "1",
                "minAmountOut": minimum.to_string(),
                "timeEstimate": 20,
                "deadline": request["deadline"],
                "timeWhenInactive": request["deadline"],
                "depositAddress": *paying.lock().unwrap()
            },
            "quoteRequest": request,
            "timestamp": "2026-09-30T00:00:00.000Z"
        });
        crate::bridge::sign_quote_response_for_tests(response, &key)
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
    clients.near = crate::bridge::NearIntentsClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        near_url,
        &pinned,
    )
    .unwrap();
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let destination = crate::bridge::BridgeDestination {
        destination_token: delivery.destination_token,
        intermediate: WETH,
        symbol: "DST".to_owned(),
        same_asset: false,
        near: Some(crate::bridge::NearAssets {
            origin_asset: "nep141:eth-weth.omft.near".to_owned(),
            destination_asset: "nep141:pol-dst.omft.near".to_owned(),
            origin_decimals: 18,
            destination_decimals: 18,
        }),
    };
    let route = crate::SwapBridgeRoute {
        clients: &clients,
        destination: crate::SwapBridgeDestination::Chain {
            destination: &destination,
            chain: chains.get(137).unwrap(),
        },
    };

    // A quote signed by another key isn't 1Click's.
    let error = fixture.issue(&owner, &orderbook, route).await.unwrap_err();
    assert!(
        error.to_string().contains("quote couldn't be verified"),
        "{error:#}"
    );
    fixture.assert_unsigned(&store, &submissions);
    *quoted.lock().unwrap() = (U256::from(999), quote_key.clone());
    assert_eq!(
        fixture.issue(&owner, &orderbook, route).await.unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(999),
        })
    );
    fixture.assert_unsigned(&store, &submissions);

    // The order is persisted with the verified quote and sent. Its response is lost, and the
    // resubmission rebuilds the identical order from the saved deposit address.
    *quoted.lock().unwrap() = (U256::from(1_000), quote_key);
    assert!(
        fixture
            .issue(&owner, &orderbook, route)
            .await
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let (_, request) = near_requests.lock().unwrap().last().cloned().unwrap();
    let sent = |field: &str| request[field].as_str().unwrap().parse::<Address>().unwrap();
    assert_eq!((sent("recipient"), sent("refundTo")), (receiver, executor));
    assert_eq!(request["dry"], false);
    assert_eq!(request["amount"], buy_amount.to_string());
    let record = fixture.record(&store);
    let saved = record.swap().unwrap().orders()[0].clone();
    let Some(BridgeOrderTerms::NearIntents(terms)) = saved.bridge().cloned() else {
        panic!("a NEAR Intents order keeps its verified quote");
    };
    assert_eq!(
        (
            terms.deposit_address,
            terms.min_amount_out,
            terms.amount_out
        ),
        (deposit_address, U256::from(1_000), U256::from(1_010))
    );
    assert_eq!(terms.deadline, request["deadline"]);
    let signed: Value = serde_json::from_str(&terms.signed_quote).unwrap();
    assert_eq!(signed["quoteRequest"], request);
    assert_eq!(saved.bounds().destination_minimum, Some(U256::from(1_000)));
    assert!(saved.post_hook().is_none());
    let [_, pre_hook] = record.issued() else {
        panic!("a NEAR Intents order issues only its pre-hook");
    };
    assert_eq!(pre_hook.purpose(), ExecutorPayloadPurpose::SwapPreHook);
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
    assert!(*persisted, "the quote is durable before the order request");
    assert_eq!(body, resent);
    let order = submitted_order(body);
    assert_eq!(
        (order.receiver, order.buyToken, order.buyAmount),
        (deposit_address, WETH, buy_amount)
    );
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert_eq!((hooks.pre.len(), hooks.post.len()), (1, 0));

    // A speed-up of the open order. A higher bridge fee alone lowers the destination minimum
    // to 990 while the order still asks for the open order's buy amount. That relaxes
    // nothing, and no quote is requested, so no deposit address is created for it.
    let delegated = fixture.review.plan().swap_executor().delegated().unwrap();
    let plan = speed_up_plan(
        &store,
        &swap_profile,
        delegated,
        std::slice::from_ref(&fixture.input),
        saved.uid(),
    )
    .unwrap();
    let lower =
        BridgeOrderFixture::review_at(plan, &swap_profile, delivery.provider, 1_000_000_000, 1_000);
    let lower_buy = lower.suggested_private_minimum();
    assert!(lower_buy < buy_amount);
    let unsigned = fixture.record(&store);
    let quotes = near_requests.lock().unwrap().len();
    assert_eq!(
        sign_order(
            &owner,
            &orderbook,
            &lower,
            buy_amount,
            &fixture.transaction,
            &fixture.input,
            Some((route, U256::from(990), None)),
        )
        .await
        .unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::SpeedUpBuyAmount {
            open: buy_amount,
            current: buy_amount,
        })
    );
    assert_eq!(near_requests.lock().unwrap().len(), quotes);
    assert_eq!(fixture.record(&store), unsigned);
    // A lower buy amount is signed with a quote of its own, which creates another deposit
    // address. Each order keeps and pays the address it signed.
    let second_address = Address::repeat_byte(0xd1);
    *deposit.lock().unwrap() = second_address;
    let SwapOrderOutcome::Submitted { uid: sped_up } = sign_order(
        &owner,
        &orderbook,
        &lower,
        lower_buy,
        &fixture.transaction,
        &fixture.input,
        Some((route, U256::from(1_000), None)),
    )
    .await
    .unwrap() else {
        panic!("the speed-up is submitted");
    };
    let record = fixture.record(&store);
    let [first, second] = record.swap().unwrap().orders() else {
        panic!("the speed-up is the second order");
    };
    assert_eq!(
        (second.uid(), second.invalidates()),
        (sped_up, Some(first.uid()))
    );
    assert!(second.bounds().buy_amount < first.bounds().buy_amount);
    let deposit_of = |order: &SwapOrderRecord| {
        let Some(BridgeOrderTerms::NearIntents(terms)) = order.bridge() else {
            panic!("a NEAR Intents order keeps its verified quote");
        };
        terms.deposit_address
    };
    assert_eq!(
        (deposit_of(first), deposit_of(second)),
        (deposit_address, second_address)
    );
    let body = submissions.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(submitted_order(&body).receiver, second_address);

    near_task.abort();
    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// A non-EVM delivery's signing-time 1Click quote names its receiver exactly as stored and
// refunds to the executor, and its order pays the quote's deposit address. When 1Click rejects
// the receiver there, nothing is signed and the error says so.
#[tokio::test]
async fn non_evm_order_quotes_its_receiver_and_stops_when_1click_rejects_it() {
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
    let receiver = "So11111111111111111111111111111111111111112".to_owned();
    let delivery = NonEvmDelivery {
        network: crate::bridge::NonEvmNetwork::Solana,
        receiver: receiver.clone(),
        asset_id: "nep141:sol.omft.near".to_owned(),
        contract: None,
        symbol: "SOL".to_owned(),
        decimals: 9,
    };
    let fixture = BridgeOrderFixture::new(
        &owner,
        &store,
        &view,
        profile,
        &swap_profile,
        SwapDelivery::NonEvmBridge(delivery.clone()),
    )
    .await;
    let executor = fixture.executor;

    let quote_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    let pinned = format!(
        "ed25519:{}",
        bs58::encode(quote_key.verifying_key().to_bytes()).into_string()
    );
    let deposit_address = Address::repeat_byte(0xd0);
    let accepts = Arc::new(Mutex::new(false));
    let answer = accepts.clone();
    let (near_url, near_requests, near_task) = spawn_bridge_stub_with(move |_, request| {
        if !*answer.lock().unwrap() {
            return (
                400,
                json!({ "message": "recipient is not valid" }).to_string(),
            );
        }
        let response = json!({
            "quote": {
                "amountIn": request["amount"],
                "amountInFormatted": "1",
                "amountInUsd": "1",
                "minAmountIn": request["amount"],
                "amountOut": "1010",
                "amountOutFormatted": "1",
                "amountOutUsd": "1",
                "minAmountOut": "1000",
                "timeEstimate": 20,
                "deadline": request["deadline"],
                "timeWhenInactive": request["deadline"],
                "depositAddress": deposit_address
            },
            "quoteRequest": request,
            "timestamp": "2026-09-30T00:00:00.000Z"
        });
        (
            200,
            crate::bridge::sign_quote_response_for_tests(response, &quote_key),
        )
    })
    .await;
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook(
        db.clone(),
        view.clone(),
        fixture.operation,
        swap_profile.settlement(),
    )
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        orderbook_url,
        1,
    )
    .unwrap();
    let mut clients = owner.swap_bridge_clients(&orderbook).unwrap();
    clients.near = crate::bridge::NearIntentsClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        near_url,
        &pinned,
    )
    .unwrap();
    let destination = crate::bridge::NonEvmBridgeDestination {
        contract: None,
        intermediate: WETH,
        symbol: "SOL".to_owned(),
        same_asset: false,
        near: crate::bridge::NearAssets {
            origin_asset: "nep141:eth-weth.omft.near".to_owned(),
            destination_asset: delivery.asset_id.clone(),
            origin_decimals: 18,
            destination_decimals: 9,
        },
    };
    let route = crate::SwapBridgeRoute {
        clients: &clients,
        destination: crate::SwapBridgeDestination::NonEvm(&destination),
    };

    let error = fixture.issue(&owner, &orderbook, route).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("NEAR Intents rejected the receiver"),
        "{error:#}"
    );
    fixture.assert_unsigned(&store, &submissions);
    let (_, request) = near_requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(request["recipient"], receiver.as_str());
    assert_eq!(
        request["refundTo"]
            .as_str()
            .unwrap()
            .parse::<Address>()
            .unwrap(),
        executor
    );

    *accepts.lock().unwrap() = true;
    assert!(matches!(
        fixture.issue(&owner, &orderbook, route).await.unwrap(),
        SwapOrderOutcome::Submitted { .. }
    ));
    let saved = fixture.record(&store).swap().unwrap().orders()[0].clone();
    assert_eq!(saved.delivery(), SwapDelivery::NonEvmBridge(delivery));
    assert!(saved.post_hook().is_none());
    let body = submissions.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(submitted_order(&body).receiver, deposit_address);

    near_task.abort();
    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
