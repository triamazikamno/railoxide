use super::*;

// A destination setup that fails is the swap's only effect: the Public account's side of the
// record stays as it was claimed, and the setup is retried with the same account.
#[tokio::test]
async fn a_failed_destination_setup_leaves_the_public_account_untouched() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let destination_chain = destination_chain_config(&rpc).await;
    let profile = destination_chain.accepted_executor_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        destination_chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
    let mut candidate = broadcaster(profile.delegate());
    candidate.chain_id = DESTINATION_CHAIN;
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let request = || claim(id, operation, 1, USDC, DESTINATION_TOKEN);
    let authorization = password();

    // A claim for the chain it pays on, without a setup fee limit, without a destination
    // minimum or for another setup need than its account has reserves nothing, and neither
    // does one from a shared Public account, whose refusal keeps its type.
    let mut same_chain = request();
    same_chain.origin_chain = DESTINATION_CHAIN;
    let mut unbounded = request();
    unbounded.approval.bounds.destination_setup_fee = None;
    let mut without_minimum = request();
    without_minimum.approval.bounds.destination_minimum = None;
    let mut existing = request();
    existing.approval.destination.setup = false;
    for refused in [same_chain, unbounded, without_minimum, existing] {
        assert!(owner.claim_public_swap(refused).is_err());
    }
    let mut shared = request();
    shared.source_scope = PublicAccountScope::Global;
    assert!(matches!(
        owner
            .claim_public_swap(shared)
            .unwrap_err()
            .downcast_ref::<ExecutorStoreError>(),
        Some(ExecutorStoreError::PublicSwapSourceShared)
    ));
    assert_eq!(store.records().unwrap(), [] as [ExecutorRecord; 0]);

    // The claim reserves the account, and its preparation derives it and binds its address
    // into the approval.
    owner.claim_public_swap(request()).unwrap();
    assert!(
        owner
            .prepare_public_swap_destination(
                operation,
                id,
                None,
                &authorization,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err(),
        "a new account needs its setup's broadcaster"
    );
    let SwapPairSide::Setup(prepared) = owner
        .prepare_public_swap_destination(
            operation,
            id,
            Some(candidate.clone()),
            &authorization,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
    else {
        panic!("a new account needs setup");
    };
    let executor = prepared.context().executor;
    let claimed = saved(&store, operation, id);
    assert_eq!(claimed.record.address(), Some(executor));
    assert_eq!(
        claimed.swap.approval().destination,
        SwapApprovedAccount {
            address: Some(executor),
            setup: true,
        }
    );

    // The setup's fee ceiling is the one approved with the swap, read from this account's use.
    let fee_token = candidate.token;
    owner
        .require_swap_destination_setup_fee(operation, fee_token, U256::from(1_000))
        .unwrap();
    let error = owner
        .require_swap_destination_setup_fee(operation, fee_token, U256::from(1_001))
        .unwrap_err();
    let exceeded = error
        .downcast_ref::<ExecutorPrivateFeeLimitExceeded>()
        .unwrap();
    assert_eq!(
        (exceeded.maximum(), exceeded.required()),
        (U256::from(1_000), U256::from(1_001))
    );
    // Another fee token is refused even for fewer base units than the limit.
    let other_token = Address::repeat_byte(0x34);
    let error = owner
        .require_swap_destination_setup_fee(operation, other_token, U256::from(999))
        .unwrap_err();
    let changed = error
        .downcast_ref::<crate::ExecutorPrivateFeeTokenChanged>()
        .unwrap();
    assert_eq!(
        (
            changed.approved_token(),
            changed.approved_amount(),
            changed.fee_token(),
            changed.required(),
        ),
        (fee_token, U256::from(1_000), other_token, U256::from(999))
    );

    // The setup is handed off and its nonce stays unconsumed.
    let signed_at =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.record_account_read(operation, signed_at).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                profile.delegate(),
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), signed_at, Vec::new()),
            ),
        )
        .unwrap();
    let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
    let pending = store
        .record_account_read(
            operation,
            ExecutorNonceObservation::new(confirmed, U256::ZERO),
        )
        .unwrap();
    assert_eq!(
        swap_setup_status(&pending, confirmed, &[], profile),
        SwapSetupStatus::Pending
    );

    // Nothing of the Public account is recorded: no path, no transaction, no deposit terms and
    // no shield.
    let after = saved(&store, operation, id);
    assert_eq!(after.swap, claimed.swap);
    assert!(after.swap.path().is_none());
    assert_eq!(after.swap.transactions(), [] as [PublicSwapTransaction; 0]);
    assert!(after.swap.bridge().is_none());
    assert_eq!(after.shields, [] as [B256; 0]);

    // The retry prepares the same account for the same use.
    let SwapPairSide::Setup(retried) = owner
        .prepare_public_swap_destination(
            operation,
            id,
            Some(candidate),
            &authorization,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
    else {
        panic!("the pending setup is retried");
    };
    assert_eq!(retried.context().executor, executor);
    assert_eq!(saved(&store, operation, id).swap, claimed.swap);

    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

/// The actual Public review path for the direct-deposit route used by delivery tests.
async fn delivery_review(
    owner: &ExecutorOwner,
    origin: &EffectiveChainConfig,
    source: Address,
    input_amount: U256,
    across: &AcrossClient,
) -> crate::PublicSwapReview {
    let registry = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let destination = crate::bridge::PublicBridgeDestination {
        destination: crate::bridge::BridgeDestination {
            destination_token: DESTINATION_TOKEN,
            intermediate: USDC,
            symbol: "USDC".into(),
            same_asset: true,
            near: None,
        },
        path: crate::bridge::PublicBridgePath::Deposit,
    };
    owner
        .review_public_swap(crate::PublicSwapReviewRequest {
            origin,
            source,
            sell: crate::bridge::PublicSellAsset::Erc20(USDC),
            sell_amount: input_amount,
            destination: &destination,
            slippage_bps: 50,
            gas_share_bps: 5_000,
            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
            orderbook: None,
            across,
            anchor_cache: None,
            token_registry: &registry,
            anchor_deviation_bps: 300,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            client: crate::OperationHttpClient::direct_for_tests(),
        })
        .await
        .unwrap()
}

// The delivery is signed against a quote requested with the handler and the real message. A
// quote below the approved destination minimum returns to review with the shield recorded and
// nothing of the Public account signed. Reviewing the real quote lowers the approved minimum
// and signs a new guarded delivery while retaining the earlier shield.
#[tokio::test]
async fn a_short_signing_time_quote_returns_the_public_swap_to_review() {
    use alloy::sol_types::SolValue as _;

    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let (mut origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
    // This RPC stub returns an allowance word directly; its default Multicall response has
    // empty return data, which cannot decode the Public review's ERC-20 allowance.
    origin_chain.rpc_route = crate::RpcChainRoute::new(1, vec![rpc.url.clone()]);
    let profile = destination_chain.accepted_executor_profile().unwrap();
    let spoke_pool = origin_chain.bridge_origin_profile().unwrap().spoke_pool();
    let handler = destination_chain
        .bridge_profile()
        .unwrap()
        .multicall_handler();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        destination_chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
    let mut candidate = broadcaster(profile.delegate());
    candidate.chain_id = DESTINATION_CHAIN;
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let authorization = password();
    // The preview stays at 1,000 while quotes with the real handler message stay at 999.
    // A new preview would therefore repeat the original approval forever.
    let (across_url, requests, across_task) = spawn_bridge_stub_with(move |path, _| {
        let has_message = url::Url::parse(&format!("http://across{path}"))
            .unwrap()
            .query_pairs()
            .any(|(name, _)| name == "message");
        (
            200,
            across_fee_quote(
                spoke_pool,
                U256::from(if has_message { 999 } else { 1_000 }),
                3 * 60 * 60,
            ),
        )
    })
    .await;
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let across = crate::bridge::AcrossClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        across_url,
    )
    .unwrap();
    let input_amount = U256::from(1_010);
    let mut request = claim(id, operation, 1, USDC, DESTINATION_TOKEN);
    let review =
        delivery_review(&owner, &origin_chain, request.source, input_amount, &across).await;
    assert_eq!(review.bridge().destination_minimum, U256::from(1_000));
    request.approval = review
        .approval(
            request.approval.destination,
            Some(setup_fee_limit(1_000)),
            true,
        )
        .unwrap();
    owner.claim_public_swap(request).unwrap();
    let executor = owner
        .prepare_public_swap_destination(
            operation,
            id,
            Some(candidate),
            &authorization,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .executor();
    let delegated = delegate_setup(&store, operation, profile);
    assert_eq!(delegated.executor(), executor);

    let valid_to = u32::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30 * 60,
    )
    .unwrap();
    let sign = || {
        owner.sign_public_swap_delivery(PublicSwapDeliverySigning {
            operation,
            swap_use: id,
            delegated,
            origin: &origin_chain,
            across: &across,
            input_amount,
            valid_to,
            authorization: &authorization,
            notes: None,
            client: crate::OperationHttpClient::direct_for_tests(),
        })
    };
    // What the latest quote request carried under `name`.
    let sent = |name: &str| {
        let (path, _) = requests.lock().unwrap().last().cloned().unwrap();
        url::Url::parse(&format!("http://across{path}"))
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap()
    };

    let PublicSwapDelivery::ReviewRequired {
        change,
        quote: Some(quote),
    } = sign().await.unwrap()
    else {
        panic!("a quote below the approved destination minimum signs no delivery");
    };
    assert_eq!(
        change,
        SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(999),
        }
    );

    // The request named the deposit, the handler as recipient and the real message: the drain
    // to the destination account, then that account's shield, with the account as fallback.
    assert_eq!(
        (
            sent("inputToken").parse::<Address>().unwrap(),
            sent("outputToken").parse::<Address>().unwrap(),
            sent("originChainId"),
            sent("destinationChainId"),
            sent("amount"),
        ),
        (
            USDC,
            DESTINATION_TOKEN,
            "1".to_owned(),
            DESTINATION_CHAIN.to_string(),
            input_amount.to_string(),
        )
    );
    assert_eq!(sent("recipient").parse::<Address>().unwrap(), handler);
    let message = sent("message").parse::<Bytes>().unwrap();
    let instructions = MulticallHandler::Instructions::abi_decode(&message).unwrap();
    let [drain, shield] = instructions.calls.as_slice() else {
        panic!("the message drains, then shields");
    };
    assert_eq!((drain.target, shield.target), (handler, executor));
    let drained = MulticallHandler::drainLeftoverTokensCall::abi_decode(&drain.callData).unwrap();
    assert_eq!(
        (drained.token, drained.destination),
        (DESTINATION_TOKEN, executor)
    );
    assert_eq!(instructions.fallbackRecipient, executor);
    assert_eq!(
        message,
        private_delivery_message(
            handler,
            DESTINATION_TOKEN,
            executor,
            shield.callData.clone(),
            Some(executor),
        )
    );

    // That shield is in the account's record under the swap's use, guarded by the approved
    // destination minimum, and the Public account has signed nothing.
    let stopped = saved(&store, operation, id);
    let [shield_hash] = stopped.shields.as_slice() else {
        panic!("one shield is recorded for the use");
    };
    let payload = stopped
        .record
        .issued()
        .iter()
        .find(|payload| payload.hash() == *shield_hash)
        .unwrap();
    assert_eq!(
        (payload.purpose(), payload.context().calldata()),
        (
            ExecutorPayloadPurpose::SwapDestinationShield,
            &shield.callData
        )
    );
    let signed = RelayAdapt7702::multicallCall::abi_decode(&shield.callData).unwrap();
    let guard = transferCall::abi_decode(&signed._calls[0].data).unwrap();
    assert_eq!(
        (guard._transfers[0].to, guard._transfers[0].value),
        (executor, U256::from(1_000))
    );
    assert!(stopped.swap.path().is_none());
    assert_eq!(
        stopped.swap.transactions(),
        [] as [PublicSwapTransaction; 0]
    );
    assert!(stopped.swap.bridge().is_none());

    // The real quote replaces the optimistic preview, without changing the persisted
    // approval. The user must explicitly accept these corrected terms before another sign.
    assert_ne!(review.gas_plan().approval_gas_limits, [] as [u64; 0]);
    // The Public account has now paid its approval. Re-review must retain only the deposit
    // gas while using the actual Across quote, at the original fee rates.
    rpc.state
        .calls
        .lock()
        .unwrap()
        .insert(USDC, Some(input_amount));
    let registry = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let corrected = owner
        .requote_public_swap_delivery(
            &review,
            &quote,
            &origin_chain,
            Address::repeat_byte(0x50),
            crate::OperationHttpClient::direct_for_tests(),
            None,
            &registry,
        )
        .await
        .unwrap();
    assert_eq!(corrected.gas_plan().approval_gas_limits, [] as [u64; 0]);
    assert_eq!(
        corrected.gas_plan().deposit_gas_limit,
        review.gas_plan().deposit_gas_limit
    );
    assert_eq!(
        corrected.gas_plan().max_gas_cost,
        public_swap_gas_plan(&origin_chain, 0, true, 1, 1)
            .unwrap()
            .max_gas_cost
    );
    assert_eq!(corrected.bridge().destination_minimum, U256::from(999));
    // The fee is priced again from the real output, not taken from Across's relay fee of 10.
    assert_eq!(
        (corrected.bridge().fee, corrected.bridge().leg),
        (Some(U256::from(11)), crate::BridgeLegPrice::SameAsset)
    );
    assert_eq!(
        corrected.bridge().private.unwrap().delivery_allowance,
        U256::ZERO
    );
    assert_eq!(
        corrected.bridge().private.unwrap().deposit_floor,
        Some(U256::ONE)
    );
    assert_eq!(
        corrected
            .bridge()
            .private
            .unwrap()
            .destination_shield_fee_bps,
        review.bridge().private.unwrap().destination_shield_fee_bps
    );
    assert_eq!(
        saved(&store, operation, id).swap.approval(),
        stopped.swap.approval()
    );
    let approval = corrected
        .approval(
            stopped.swap.approval().destination,
            stopped.swap.approval().bounds.destination_setup_fee,
            true,
        )
        .unwrap();
    // The destination's bound address and its original setup need cannot change on review.
    for destination in [
        SwapApprovedAccount {
            address: None,
            ..approval.destination
        },
        SwapApprovedAccount {
            setup: false,
            ..approval.destination
        },
    ] {
        assert!(
            owner
                .reapprove_public_swap(
                    operation,
                    id,
                    PublicSwapApproval {
                        destination,
                        ..approval.clone()
                    }
                )
                .is_err()
        );
    }
    let reapproved = owner
        .reapprove_public_swap(operation, id, approval.clone())
        .unwrap();
    assert_eq!(
        reapproved.public_swap_use(id).unwrap().1.approval(),
        &approval
    );
    assert_eq!(reapproved.issued(), stopped.record.issued());

    // The unchanged quote now covers the approved minimum and returns fresh terms, paid to
    // the handler with the hash of the new message the request carried.
    let PublicSwapDelivery::Signed {
        delivery,
        message,
        terms,
    } = sign().await.unwrap()
    else {
        panic!("the quote covers the freshly reviewed minimum");
    };
    assert_eq!(sent("message").parse::<Bytes>().unwrap(), message);
    assert_eq!(
        (terms.recipient, terms.message_hash),
        (Some(handler), Some(keccak256(&message)))
    );
    assert_eq!(
        (terms.input_amount, terms.output_amount),
        (input_amount, U256::from(999))
    );
    assert_eq!(
        (terms.spoke_pool, terms.input_token, terms.output_token),
        (spoke_pool, USDC, DESTINATION_TOKEN)
    );
    assert_eq!(
        (
            delivery.handler,
            delivery.destination_executor,
            delivery.fallback
        ),
        (handler, executor, Some(executor))
    );
    // The returned shield is recorded too, and the path is still the Public account's to sign.
    let signed = saved(&store, operation, id);
    assert!(signed.shields.contains(shield_hash));
    assert_ne!(delivery.shield_multicall, shield.callData);
    let new_shield = RelayAdapt7702::multicallCall::abi_decode(&delivery.shield_multicall).unwrap();
    let guard = transferCall::abi_decode(&new_shield._calls[0].data).unwrap();
    assert_eq!(guard._transfers[0].value, U256::from(999));
    assert!(signed.record.issued().iter().any(|payload| {
        signed.shields.contains(&payload.hash())
            && *payload.context().calldata() == delivery.shield_multicall
    }));
    assert!(signed.swap.path().is_none());

    across_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// An order's signing-time quote below the approved destination minimum is taken again for a
// raised buy amount when the raise is within a fifth of the approved allowed gas: the terms
// are for that amount and the approved minimum stands. A larger shortfall returns the swap to
// review with the first quote's minimum, and Across isn't asked again.
#[tokio::test]
async fn a_public_orders_short_signing_time_quote_within_the_cushion_raises_its_buy_amount() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let (mut origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
    // This RPC stub returns an allowance word directly; its default Multicall response has
    // empty return data, which cannot decode the Public review's ERC-20 allowance.
    origin_chain.rpc_route = crate::RpcChainRoute::new(1, vec![rpc.url.clone()]);
    let profile = destination_chain.accepted_executor_profile().unwrap();
    let spoke_pool = origin_chain.bridge_origin_profile().unwrap().spoke_pool();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        destination_chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
    let mut candidate = broadcaster(profile.delegate());
    candidate.chain_id = DESTINATION_CHAIN;
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let authorization = password();
    // The order buys at least 1,010 for a destination minimum of 1,000, and its approval
    // allows 100 for gas: a cushion of 20.
    let mut request = claim(id, operation, 1, USDC, DESTINATION_TOKEN);
    request.intent.order = true;
    request.approval.sell_token = ORDER_SELL_TOKEN;
    request.approval.bounds.buy_amount = U256::from(1_010);
    request.approval.bounds.gas_allowance = Some(U256::from(100));
    owner.claim_public_swap(request).unwrap();
    let executor = owner
        .prepare_public_swap_destination(
            operation,
            id,
            Some(candidate),
            &authorization,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .executor();
    let delegated = delegate_setup(&store, operation, profile);
    assert_eq!(delegated.executor(), executor);

    // Across answers each quote with the next of the outputs the test sets.
    let outputs = Arc::new(Mutex::new(Vec::<U256>::new()));
    let answers = outputs.clone();
    let (across_url, requests, across_task) = spawn_bridge_stub(move |_| {
        across_fee_quote(spoke_pool, answers.lock().unwrap().remove(0), 3 * 60 * 60)
    })
    .await;
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let across = crate::bridge::AcrossClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        across_url,
    )
    .unwrap();
    let valid_to = u32::try_from(unix_now() + 30 * 60).unwrap();
    let sign = || {
        owner.sign_public_swap_delivery(PublicSwapDeliverySigning {
            operation,
            swap_use: id,
            delegated,
            origin: &origin_chain,
            across: &across,
            input_amount: U256::from(1_010),
            valid_to,
            authorization: &authorization,
            notes: None,
            client: crate::OperationHttpClient::direct_for_tests(),
        })
    };
    // The amount and the message of every quote request so far.
    let quoted = || -> Vec<(String, String)> {
        requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| {
                let url = url::Url::parse(&format!("http://across{path}")).unwrap();
                let sent = |name: &str| {
                    url.query_pairs()
                        .find(|(key, _)| key == name)
                        .map(|(_, value)| value.into_owned())
                        .unwrap()
                };
                (sent("amount"), sent("message"))
            })
            .collect()
    };

    // 900 for 1,010 needs a deposit of 1,124, which is 114 more: beyond the cushion.
    *outputs.lock().unwrap() = vec![U256::from(900)];
    let PublicSwapDelivery::ReviewRequired { change, quote } = sign().await.unwrap() else {
        panic!("a shortfall beyond the cushion signs no delivery");
    };
    assert_eq!(
        change,
        SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(900),
        }
    );
    assert!(quote.is_some());
    assert_eq!(quoted().len(), 1);

    // Even when the raised amount is still short, the returned change is for the original
    // input. Returning the raised quote would associate its output with the wrong review.
    *outputs.lock().unwrap() = vec![U256::from(999), U256::from(998)];
    let PublicSwapDelivery::ReviewRequired {
        change,
        quote: Some(quote),
    } = sign().await.unwrap()
    else {
        panic!("a raised quote that still falls short returns to review");
    };
    assert_eq!(
        change,
        SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(999),
        }
    );
    assert_eq!(quoted().len(), 3);
    let (preview_url, _, preview_task) =
        spawn_bridge_stub(move |_| across_fee_quote(spoke_pool, U256::from(1_000), 3 * 60 * 60))
            .await;
    let preview = AcrossClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        preview_url,
    )
    .unwrap();
    // The original source order needed an approval. Its updated review must read the
    // allowance again after that transaction is paid, so no ETH is needed for another one.
    rpc.state.codes.lock().unwrap().insert(
        origin_chain.public_swap_profile().unwrap().math(),
        swap_math_runtime_code(),
    );
    let (orderbook_url, _, orderbook_task) = spawn_bridge_stub(move |_| {
        json!({
            "quote": {
                "sellToken": ORDER_SELL_TOKEN, "buyToken": USDC,
                "sellAmount": "1010", "buyAmount": "1010", "validTo": 1,
                "feeAmount": "0", "gasAmount": "0", "gasPrice": "0",
                "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        })
        .to_string()
    })
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        orderbook_url,
        1,
    )
    .unwrap();
    let destination = crate::bridge::PublicBridgeDestination {
        destination: crate::bridge::BridgeDestination {
            destination_token: DESTINATION_TOKEN,
            intermediate: USDC,
            symbol: "USDC".into(),
            same_asset: true,
            near: None,
        },
        path: crate::bridge::PublicBridgePath::Order,
    };
    let registry = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let review = owner
        .review_public_swap(crate::PublicSwapReviewRequest {
            origin: &origin_chain,
            source: Address::repeat_byte(0x50),
            sell: crate::bridge::PublicSellAsset::Erc20(ORDER_SELL_TOKEN),
            sell_amount: U256::from(1_010),
            destination: &destination,
            slippage_bps: 0,
            gas_share_bps: 0,
            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
            orderbook: Some(&orderbook),
            across: &preview,
            anchor_cache: None,
            token_registry: &registry,
            anchor_deviation_bps: 300,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            client: crate::OperationHttpClient::direct_for_tests(),
        })
        .await
        .unwrap();
    assert_eq!(review.buy_amount(), Some(U256::from(1_010)));
    assert_ne!(review.gas_plan().approval_gas_limits, [] as [u64; 0]);
    let saved_approval = saved(&store, operation, id).swap.approval().clone();
    rpc.state
        .calls
        .lock()
        .unwrap()
        .insert(ORDER_SELL_TOKEN, Some(U256::MAX));
    let corrected = owner
        .requote_public_swap_delivery(
            &review,
            &quote,
            &origin_chain,
            Address::repeat_byte(0x50),
            crate::OperationHttpClient::direct_for_tests(),
            None,
            &registry,
        )
        .await
        .unwrap();
    assert_eq!(corrected.gas_plan().approval_gas_limits, [] as [u64; 0]);
    assert_eq!(corrected.gas_plan().deposit_gas_limit, None);
    assert_eq!(corrected.gas_plan().max_gas_cost, U256::ZERO);
    assert_eq!(
        corrected.gas_plan().max_fee_per_gas,
        review.gas_plan().max_fee_per_gas
    );
    assert_eq!(
        corrected.gas_plan().max_priority_fee_per_gas,
        review.gas_plan().max_priority_fee_per_gas
    );
    assert_eq!(corrected.quote(), review.quote());
    assert_eq!(corrected.buy_amount(), review.buy_amount());
    assert_eq!(corrected.bridge().destination_minimum, U256::from(999));
    assert_eq!(
        saved(&store, operation, id).swap.approval(),
        &saved_approval
    );
    orderbook_task.abort();
    let raised_review = delivery_review(
        &owner,
        &origin_chain,
        Address::repeat_byte(0x50),
        U256::from(1_013),
        &preview,
    )
    .await;
    assert!(
        owner
            .requote_public_swap_delivery(
                &raised_review,
                &quote,
                &origin_chain,
                Address::repeat_byte(0x50),
                crate::OperationHttpClient::direct_for_tests(),
                None,
                &registry,
            )
            .await
            .is_err()
    );
    preview_task.abort();

    // 999 for 1,010 needs ceil(1,010 * 1,000 / 999) = 1,012 and a margin of 1: 3 more, within
    // the cushion. Across quotes 1,013 with the same message, and its 1,002 covers the minimum.
    *outputs.lock().unwrap() = vec![U256::from(999), U256::from(1_002)];
    let PublicSwapDelivery::Signed { message, terms, .. } = sign().await.unwrap() else {
        panic!("a shortfall within the cushion signs the delivery");
    };
    assert_eq!(
        (terms.input_amount, terms.output_amount),
        (U256::from(1_013), U256::from(1_000))
    );
    assert_eq!(terms.message_hash, Some(keccak256(&message)));
    let quoted = quoted();
    let [
        _,
        _,
        _,
        (first_amount, first_message),
        (raised_amount, raised_message),
    ] = quoted.as_slice()
    else {
        panic!("the delivery was quoted twice more");
    };
    assert_eq!(
        (first_amount.as_str(), raised_amount.as_str()),
        ("1010", "1013")
    );
    assert_eq!(first_message, raised_message);
    assert_eq!(raised_message.parse::<Bytes>().unwrap(), message);
    // The Public account has still signed nothing.
    assert!(saved(&store, operation, id).swap.path().is_none());

    across_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// A restart after the destination setup is confirmed, before the Public account approves
// anything, resumes the same claim: the delegation is confirmed from the chain again and the
// swap's record is as it was.
#[tokio::test]
async fn a_restart_between_setup_and_approval_resumes_the_public_swap_unsigned() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    // The destination chain is the mock chain, whose nonce is past the setup's.
    let mut config =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let profile = config.accepted_executor_profile().unwrap();
    let railgun = config.require_railgun().unwrap().deployment.contract;
    let mut mock = MockChain::new(1, railgun, profile.delegate());
    mock.head = 14;
    mock.nonces = vec![(11, 1)];
    let served = Arc::new(Mutex::new(mock));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| served.lock().unwrap().respond(&request)),
        Arc::default(),
        Arc::default(),
    )
    .await;
    config.finality_depth = 1;
    config.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
    let start = |generation| {
        ExecutorOwner::new(
            generation,
            db.clone(),
            view.clone(),
            config.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap()
    };
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let request = || claim(id, operation, DESTINATION_CHAIN, DESTINATION_TOKEN, USDC);

    // The claimed account is the mock chain's executor. Its setup's nonce 0 reads as consumed
    // from block 11 on.
    let owner = start(0);
    owner.claim_public_swap(request()).unwrap();
    store.bind_address(operation, EXECUTOR).unwrap();
    store
        .bind_public_swap_destination(operation, id, EXECUTOR)
        .unwrap();
    let before_setup = ExecutorNonceObservation::new(BlockNumHash::new(10, B256::ZERO), U256::ZERO);
    store.record_account_read(operation, before_setup).unwrap();
    let setup_call = execute(vec![private_transaction(0x11, 0x12)], Vec::new(), 0);
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                profile.delegate(),
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(setup_call, before_setup, Vec::new()),
            ),
        )
        .unwrap();
    let reconciled = owner
        .reconcile_account(operation, crate::OperationHttpClient::direct_for_tests())
        .await
        .unwrap();
    assert_eq!(
        reconciled.record().payload_state(setup),
        Some(ExecutorPayloadState::Resolved)
    );
    let before = saved(&store, operation, id);
    owner.shutdown().await;
    drop(owner);

    // The same request returns the same claim after the restart, and the destination account
    // is confirmed delegated at the confirmed block.
    let restarted = start(1);
    assert_eq!(
        restarted.claim_public_swap(request()).unwrap(),
        before.record
    );
    let delegated = restarted
        .delegated_public_swap_destination(
            operation,
            13,
            id,
            None,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            delegated.operation(),
            delegated.executor(),
            delegated.setup_payload()
        ),
        (operation, EXECUTOR, setup)
    );

    // Nothing of the swap changed, and the Public account has no transaction.
    let after = saved(&store, operation, id);
    assert_eq!(after.swap, before.swap);
    assert!(after.swap.path().is_none());
    assert_eq!(after.swap.transactions(), [] as [PublicSwapTransaction; 0]);
    assert_eq!(after.shields, [] as [B256; 0]);

    server.abort();
    restarted.shutdown().await;
    drop(restarted);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
