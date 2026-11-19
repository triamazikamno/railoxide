use super::*;

#[tokio::test]
async fn swap_quote_uses_background_prices_without_fee_or_anchor_reads() {
    #[derive(serde::Serialize)]
    struct LegacyApproval<'a> {
        bounds: &'a SwapApprovedBounds,
        price_acknowledged: bool,
    }

    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let gas_sampled = Arc::new(tokio::sync::Notify::new());
    let response_ready = gas_sampled.clone();
    let (endpoint, rpc_task) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            assert_eq!(
                request["method"], "eth_gasPrice",
                "quotes need no fee or anchor RPC"
            );
            gas_sampled.notify_one();
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x3b9aca00"})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let settings = crate::settings::WalletSettings::default();
    let mut chain = crate::settings::build_effective_chain_configs(&settings)
        .unwrap()
        .get(1)
        .unwrap()
        .clone();
    chain.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
    let profile = chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let amount = U256::from(1_000_000_000_000_000_000_u64);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            WETH,
            amount,
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
    };
    let SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
        &builder,
        &profile,
        owner.swap_preview_executor().unwrap(),
        &[input],
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: USDC,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("one note fits");
    };
    // A 0.1 WETH CoW fee makes the net output more than 3% below the anchor, but the
    // trading rate itself is fair. Explicit fees must not fail the exchange-rate check.
    // Withhold the quote until the gas request arrives: awaiting the quote before starting
    // the RPC would stall, even though both requests are independent.
    let (quote_url, quotes, quote_task) = spawn_quote_stub(
        json!({
            "quote": {
                "sellToken": WETH, "buyToken": USDC, "sellAmount": "897500000000000000",
                "buyAmount": "2692500000", "validTo": 1, "feeAmount": "100000000000000000", "gasAmount": "0",
                "gasPrice": "0", "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }),
        Some(response_ready),
    )
    .await;
    let cache = crate::TokenAnchorRateCache::new();
    cache.store_rate(1, WETH, amount);
    cache.store_rate(1, USDC, U256::from(3_000_000_000_u64));
    let tokens = crate::settings::build_effective_token_registry(&settings).unwrap();
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        quote_url,
        1,
    )
    .unwrap();
    let review = tokio::time::timeout(
        Duration::from_secs(5),
        owner.review_swap(SwapReviewRequest {
            plan: plan.clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(10),
            orderbook: &orderbook,
            anchor_cache: Some(&cache),
            token_registry: &tokens,
            bridge: None,
        }),
    )
    .await
    .expect("gas price must be queried without waiting for the quote response")
    .unwrap();
    assert!(
        matches!(review.price(), SwapPrice::Verified { rate, observations }
        if rate.buy_rate == U256::from(3_000_000_000_u64) && observations.is_empty())
    );
    // 1 gwei from RPC with the swap's 25% cushion, at 3,000 USDC/ETH.
    // Applying the broadcaster buffer as well would inflate this allowance.
    assert_eq!(
        review.gas_estimate(),
        (U256::from(plan.hook_gas_estimate()) * U256::from(15)).div_ceil(U256::from(4))
    );
    // Configured oracles with no cached rates still produce a quote. The user must
    // explicitly accept the missing independent check before it can be approved.
    let unverified = owner
        .review_swap(SwapReviewRequest {
            plan: plan.clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(10),
            orderbook: &orderbook,
            anchor_cache: Some(&crate::TokenAnchorRateCache::new()),
            token_registry: &tokens,
            bridge: None,
        })
        .await
        .unwrap();
    assert_eq!(unverified.price(), &SwapPrice::Unverified);
    let unverified_minimum = unverified.suggested_private_minimum();
    assert!(unverified.approval(unverified_minimum, false).is_err());
    assert!(unverified.approval(unverified_minimum, true).is_ok());
    // A failed fresh check must not be upgraded back to verified by the old cache
    // when returning to review. The quote and its economic terms remain available.
    let retry = owner
        .review_swap(SwapReviewRequest {
            plan: plan.clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(10),
            orderbook: &orderbook,
            anchor_cache: None,
            token_registry: &tokens,
            bridge: None,
        })
        .await
        .unwrap();
    assert_eq!(retry.price(), &SwapPrice::Unverified);
    assert_eq!(
        retry.suggested_private_minimum(),
        review.suggested_private_minimum()
    );
    assert!(
        retry
            .approval(retry.suggested_private_minimum(), false)
            .is_err()
    );
    assert_eq!(
        quotes.lock().unwrap()[0]["sellAmountBeforeFee"],
        "997500000000000000"
    );
    // Raising the anchor makes the trading rate itself poor. A large explicit fee must
    // not exempt that quote from the independent rate protection.
    cache.store_rate(1, USDC, U256::from(3_200_000_000_u64));
    let error = owner
        .review_swap(SwapReviewRequest {
            plan,
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(10),
            orderbook: &orderbook,
            anchor_cache: Some(&cache),
            token_registry: &tokens,
            bridge: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<crate::QuoteDeviationError>(),
        Some(&crate::QuoteDeviationError::ExceedsThreshold)
    );
    // Cached checks have no block observations. They must survive restart as checked, so the
    // same review doesn't ask for approval again after setup.
    let approval = review
        .approval(review.suggested_private_minimum(), false)
        .unwrap();
    let restored = rmp_serde::from_slice(&rmp_serde::to_vec_named(&approval).unwrap()).unwrap();
    assert_eq!(review.approval_change(&restored), None);
    let mut unchecked = restored;
    unchecked.price_verified = Some(false);
    assert_eq!(
        review.approval_change(&unchecked),
        Some(SwapReviewChange::PriceVerification)
    );
    // Old approvals have no explicit verification marker; their recorded observations
    // still distinguish a checked price from an acknowledged, unverified one.
    let mut legacy_bounds = approval.bounds;
    legacy_bounds.anchors = vec![SwapAnchorObservation {
        source: Address::repeat_byte(1),
        block: alloy::eips::BlockNumHash::new(10, B256::repeat_byte(2)),
        block_timestamp: 100,
        updated_at: Some(100),
    }];
    let restore_legacy = |bounds: &SwapApprovedBounds| {
        rmp_serde::from_slice(
            &rmp_serde::to_vec_named(&LegacyApproval {
                bounds,
                price_acknowledged: bounds.anchors.is_empty(),
            })
            .unwrap(),
        )
        .unwrap()
    };
    let old = restore_legacy(&legacy_bounds);
    assert_eq!(review.approval_change(&old), None);
    legacy_bounds.anchors.clear();
    let old = restore_legacy(&legacy_bounds);
    assert_eq!(
        review.approval_change(&old),
        Some(SwapReviewChange::PriceVerification)
    );
    quote_task.abort();
    rpc_task.abort();
    drop(owner);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// Proving is replaced by a hand-built pre-hook transaction that spends the planned note; the
// signing, persistence, and submission path is the one `submit_swap_order` runs after proving.
#[tokio::test]
async fn swap_order_is_signed_for_current_terms_and_persisted_before_submission() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let setup_chain = chain(&rpc);
    let profile = setup_chain.accepted_executor_profile().unwrap();
    let delegate = profile.delegate();
    let setup_owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        setup_chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let executor = setup_owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .context()
        .executor;
    setup_owner.shutdown().await;
    drop(setup_owner);

    // The setup wins nonce 0, so the pre-hook signs at k = 1 and the post-hook at k + 1.
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let before =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.record_account_read(operation, before).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), before, Vec::new()),
            ),
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    let record = store.record_account_read(operation, observed).unwrap();
    let code = [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        delegate.as_slice(),
    ]
    .concat();
    let SwapSetupStatus::Delegated(delegated) =
        swap_setup_status(&record, observed.block(), &code, profile)
    else {
        panic!("the setup delegated the executor");
    };

    // Signing must use cached anchors even with configured oracles and the shared Railgun fee
    // constant. Only the quote's gas price may reach this endpoint; no fee, head or oracle
    // request is needed.
    let unexpected_reads = Arc::new(AtomicU64::new(0));
    let unexpected = unexpected_reads.clone();
    let (fee_endpoint, fee_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            if request["method"] != "eth_gasPrice" {
                unexpected.fetch_add(1, Ordering::Relaxed);
                return json!({"jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -32601, "message": "chain reads unavailable"}});
            }
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let mut swap_chain = chains.get(1).cloned().unwrap();
    swap_chain.rpc_route = crate::RpcChainRoute::new(1, vec![fee_endpoint]);
    let swap_profile = swap_chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        swap_chain.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();

    let amount = U256::from(1_000_000);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            WETH,
            amount,
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
    };
    let SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
        &builder,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: USDC,
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
    let (quote_url, quotes, quote_task) = spawn_quote_stub(
        json!({
            "quote": {
                "sellToken": WETH, "buyToken": USDC, "sellAmount": "996500", "buyAmount": "3000000000",
                "validTo": 1, "feeAmount": "1000", "gasAmount": "0", "gasPrice": "0",
                "sellTokenPrice": "1000000000000", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }),
        None,
    )
    .await;
    let tokens = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let anchors = crate::TokenAnchorRateCache::new();
    anchors.store_rate(1, WETH, U256::from(997_500));
    anchors.store_rate(1, USDC, U256::from(3_000_000_000_u64));
    // The user chose a 30-minute validity for this Private delivery swap.
    let quoted_from = unix_now();
    let review = owner
        .review_swap(SwapReviewRequest {
            plan,
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(30),
            orderbook: &CowOrderbookClient::new(
                OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
                quote_url,
                1,
            )
            .unwrap(),
            anchor_cache: Some(&anchors),
            token_registry: &tokens,
            bridge: None,
        })
        .await
        .unwrap();
    let quoted_until = unix_now();
    quote_task.abort();
    assert!(matches!(review.price(), SwapPrice::Verified { .. }));
    assert_eq!(review.valid_for(), Duration::from_mins(30));
    let quote_valid_to = quotes.lock().unwrap()[0]["validTo"].as_u64().unwrap();
    assert!((quoted_from + 1_800..=quoted_until + 1_800).contains(&quote_valid_to));
    // Railgun keeps 0.25% of the 1,000,000 the pre-hook unshields. The quote prices what the
    // executor then holds, and the order sells it.
    let sell_amount = U256::from(997_500);
    assert_eq!(review.sell_amount(), sell_amount);
    assert_eq!(
        quotes.lock().unwrap()[0]["sellAmountBeforeFee"],
        json!(sell_amount.to_string())
    );
    let private_minimum = review.suggested_private_minimum();
    let buy_amount = review.buy_amount_for(private_minimum).unwrap();

    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook_with_lost_response(
        db.clone(),
        view.clone(),
        operation,
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
    let pre_hook_transaction = Transaction {
        proof: SnarkProof::default(),
        merkleRoot: B256::ZERO,
        nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
        commitments: vec![B256::ZERO],
        boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
        unshieldPreimage: CommitmentPreimage::empty(),
    };
    let authorization = password();
    let output_pois = OutputPois {
        submissions: submissions.clone(),
        ..OutputPois::default()
    };
    let change = change_output_poi(pre_hook_transaction.commitments[0]);
    let issue = |transactions: Vec<Transaction>,
                 change_output_pois: Vec<local_db::PendingOutputPoiContextRecord>| {
        owner.issue_swap_order(crate::SwapOrderSigning {
            review: &review,
            swap_use: crate::vault::SwapUseId::first(review.plan().operation().unwrap()),
            private_minimum,
            price_acknowledged: true,
            transactions,
            inputs: std::slice::from_ref(&input),
            change_output_pois,
            output_pois: &output_pois,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &anchors,
            token_registry: &tokens,
            bridge: None,
            destination_minimum: None,
            destination: None,
            continuing_minimum: None,
        })
    };
    let first = || issue(vec![pre_hook_transaction.clone()], vec![change.clone()]);

    // A stalled output PPOI commit must not lock out another operation. If the selected
    // account changes during that wait, the late result must never persist or submit signed
    // data.
    {
        let gate = StalledOutputPois::default();
        let preparing = ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            swap_chain,
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        let pending = preparing.issue_swap_order(crate::SwapOrderSigning {
            review: &review,
            swap_use: crate::vault::SwapUseId::first(review.plan().operation().unwrap()),
            private_minimum,
            price_acknowledged: true,
            transactions: vec![pre_hook_transaction.clone()],
            inputs: std::slice::from_ref(&input),
            change_output_pois: vec![change.clone()],
            output_pois: &gate,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &anchors,
            token_registry: &tokens,
            bridge: None,
            destination_minimum: None,
            destination: None,
            continuing_minimum: None,
        });
        tokio::pin!(pending);
        tokio::select! {
            () = gate.started.notified() => {},
            result = &mut pending => panic!("the PPOI commit did not wait: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(5)) => panic!("the commit never started"),
        }
        let unrelated = tokio::time::timeout(
            Duration::from_secs(2),
            preparing.reconcile_account(
                ExecutorOperationId::random().unwrap(),
                crate::OperationHttpClient::direct_for_tests(),
            ),
        )
        .await
        .expect("network preparation must release activity");
        assert!(unrelated.is_err(), "the unrelated operation does not exist");
        store.invalidate_observation(operation).unwrap();
        gate.release.notify_one();
        let error = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("changed during preparation"),
            "{error:#}"
        );
        assert!(submissions.lock().unwrap().is_empty());
        assert!(
            store
                .records()
                .unwrap()
                .iter()
                .find(|record| record.operation() == operation)
                .unwrap()
                .swap()
                .is_none()
        );
        preparing.shutdown().await;
        store.record_account_read(operation, observed).unwrap();
    }
    let signed_from = unix_now();
    assert!(
        first()
            .await
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let signed_until = unix_now();
    // Reload through a new store handle. The signature survives interruption, and no
    // signing authorization or proof is needed to resend the exact original request.
    let restored_store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let restored = restored_store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let saved = restored.swap().unwrap().orders().last().unwrap();
    assert_eq!(saved.submission_status(), SwapSubmissionStatus::Pending);
    assert_eq!(
        (saved.bounds().sell_amount, saved.bounds().spend_amount()),
        (sell_amount, amount)
    );
    // A release frees the unsent order's notes, but resending the order reserves them again,
    // and only while no other operation took them after the release.
    let pre_hook_inputs = vec![ExecutorInputIdentity::from_utxo(&input)];
    assert_eq!(restored.reserved_inputs(), pre_hook_inputs);
    owner.release_input_lock(operation).unwrap();
    let released = restored_store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(released.reserved_inputs(), [] as [ExecutorInputIdentity; 0]);
    let other = ExecutorOperationId::random().unwrap();
    // The reservation takes the prefetched spare, which already has its address.
    let other_address = store
        .reserve(other, delegate, None, &[])
        .unwrap()
        .address()
        .unwrap_or(Address::repeat_byte(9));
    store.bind_address(other, other_address).unwrap();
    store.record_account_read(other, before).unwrap();
    store
        .record_issued(
            other,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                B256::repeat_byte(0x55),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"other"),
                    before,
                    pre_hook_inputs.clone(),
                ),
            ),
        )
        .unwrap();
    assert_eq!(
        owner
            .resubmit_swap_order(operation, &orderbook)
            .await
            .unwrap_err()
            .to_string(),
        "another operation now uses this order's notes; wait for the order to expire"
    );
    owner.release_input_lock(other).unwrap();
    let SwapOrderOutcome::Submitted { uid } = owner
        .resubmit_swap_order(operation, &orderbook)
        .await
        .unwrap()
    else {
        panic!("the duplicate order is accepted");
    };
    assert_eq!(uid, saved.uid());
    let submitted = submissions.lock().unwrap().clone();
    let [(persisted, body), (resent_persisted, resent)] = submitted.as_slice() else {
        panic!("one initial request and one resubmission");
    };
    assert!(*resent_persisted);
    assert_eq!(
        body, resent,
        "the order, signature, quote and appData are unchanged"
    );
    let accepted = restored_store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(
        accepted.swap().unwrap().orders()[0].submission_status(),
        SwapSubmissionStatus::Accepted
    );
    assert_eq!(accepted.reserved_inputs(), pre_hook_inputs);
    // Accepted requests are idempotent locally, too.
    assert_eq!(
        owner
            .resubmit_swap_order(operation, &orderbook)
            .await
            .unwrap(),
        SwapOrderOutcome::Submitted { uid }
    );
    assert!(
        *persisted,
        "the attempt is durable before the order request arrives"
    );
    // The change output's PPOI context reached the wallet before the order request, keyed by
    // its commitment alone, so chain observation of any settlement submits it.
    {
        let commits = output_pois.commits.lock().unwrap();
        let [(requests, contexts)] = commits.as_slice() else {
            panic!("the change-output contexts are committed once");
        };
        assert_eq!(*requests, 0);
        let [context] = contexts.as_slice() else {
            panic!("one change output");
        };
        assert_eq!(context.output_commitment, change.output_commitment);
        assert!(context.observation.is_none() && context.txid_merkleroot_index.is_none());
    }
    // A fill-or-kill sell order that the executor owns, signs, and receives.
    let order = submitted_order(body);
    assert_eq!(
        (
            order.kind.as_str(),
            order.partiallyFillable,
            order.feeAmount
        ),
        (ORDER_KIND_SELL, false, U256::ZERO)
    );
    assert_eq!((order.receiver, uid.owner()), (executor, executor));
    assert_eq!(order.buyAmount, buy_amount);
    // The 30-minute approval expires 30 minutes after signing, and the record keeps it.
    assert!((signed_from + 1_800..=signed_until + 1_800).contains(&u64::from(order.validTo)));
    assert_eq!(saved.bounds().valid_for_secs, Some(1_800));
    let signature = body["signature"]
        .as_str()
        .unwrap()
        .parse::<Bytes>()
        .unwrap();
    let signature: [u8; 65] = signature[..].try_into().unwrap();
    assert_eq!(
        recover_order_signer(
            &signature,
            &order_digest(&order, 1, swap_profile.settlement())
        )
        .unwrap(),
        executor
    );
    // Both hooks call the executor, and the post-hook's balance guard is the buy amount.
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert!(
        hooks
            .pre
            .iter()
            .chain(&hooks.post)
            .all(|hook| hook.target == executor)
    );
    let post_hook = RelayAdapt7702::multicallCall::abi_decode(&hooks.post[0].call_data).unwrap();
    let guard = transferCall::abi_decode(&post_hook._calls[0].data).unwrap();
    assert_eq!(guard._transfers[0].value, buy_amount);
    // The order and the pre-hook's exact approval both cover what the unshield leaves, so
    // the executor's balance and allowance after the pre-hook match `sellAmount`.
    assert_eq!(order.sellAmount, sell_amount);
    let pre_hook = RelayAdapt7702::executeCall::abi_decode(&hooks.pre[0].call_data).unwrap();
    let approval =
        approveCall::abi_decode(&pre_hook._actionData.calls.last().unwrap().data).unwrap();
    assert_eq!(
        (approval.spender, approval.amount),
        (swap_profile.vault_relayer(), sell_amount)
    );

    // Nothing is signed or sent while the first attempt's pre-hook can still execute, nor
    // after it executed: that order can still fill until `validTo`.
    assert_eq!(
        first().await.unwrap_err().to_string(),
        "the previous order of this swap can still execute; retry once it has ended"
    );
    let executed = SwapOrderObservations {
        pre_hook_executed: Some(SwapObservation {
            block: observed.block(),
            transaction_hash: Some(B256::repeat_byte(0x44)),
        }),
        ..SwapOrderObservations::default()
    };
    store
        .record_swap_observations(operation, uid, executed)
        .unwrap();
    assert_eq!(
        first().await.unwrap_err().to_string(),
        "the previous order of this swap can still execute; retry once it has ended"
    );
    assert_eq!(submissions.lock().unwrap().len(), 2);
    assert_eq!(output_pois.commits.lock().unwrap().len(), 1);

    // Once the pre-hook is dead at its unused nonce, a retry for the same notes and amount
    // keeps the proof: no prover runs, and its change outputs already have PPOI contexts.
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
        .record_swap_observations(operation, uid, expired.clone())
        .unwrap();
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let digest = record.swap().unwrap().proof().digest();
    let (reused, _) =
        crate::reusable_swap_proof(&record, review.plan(), std::slice::from_ref(&input))
            .expect("same notes and amount reuse the proof");
    assert_eq!(
        alloy::sol_types::SolValue::abi_encode(&reused),
        alloy::sol_types::SolValue::abi_encode(&vec![pre_hook_transaction.clone()])
    );
    // A different amount is proved again.
    let SwapAmountPlan::Fits(smaller) = crate::plan_swap_inputs(
        &builder,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: USDC,
            amount: amount / U256::from(2),
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        swap_profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("half the note fits one order");
    };
    assert!(crate::reusable_swap_proof(&record, &smaller, std::slice::from_ref(&input)).is_none());
    // An expired order can't fill, so the retry's pre-hook invalidates nothing.
    assert_eq!(
        crate::swap_invalidation(&record, &swap_profile, std::time::SystemTime::now()).unwrap(),
        None
    );
    // A real retry starts after `validTo`; here the next second gives it a new deadline.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let SwapOrderOutcome::Submitted { uid: retry } = issue(reused, Vec::new()).await.unwrap()
    else {
        panic!("the retry is submitted");
    };
    assert_ne!(retry, uid);
    let submitted = submissions.lock().unwrap().clone();
    assert_eq!(submitted.len(), 3);
    let hooks = serde_json::from_str::<AppData>(submitted[2].1["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    let pre_hook = RelayAdapt7702::executeCall::abi_decode(&hooks.pre[0].call_data).unwrap();
    assert_eq!(
        pre_hook._actionData.calls.len(),
        2,
        "deadline and approval only"
    );
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(record.swap().unwrap().proof().digest(), digest);
    assert_eq!(record.swap().unwrap().orders().len(), 2);
    assert_eq!(output_pois.commits.lock().unwrap().len(), 1);

    // After expiry, the same account can sign a reviewed order for another pair.
    store
        .record_swap_observations(operation, retry, expired)
        .unwrap();
    let dai = alloy::primitives::address!("6b175474e89094c44da98b954eedeac495271d0f");
    let SwapAmountPlan::Fits(new_plan) = crate::plan_swap_inputs(
        &builder,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: dai,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        swap_profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("the new pair fits");
    };
    assert!(crate::reusable_swap_proof(&record, &new_plan, std::slice::from_ref(&input)).is_none());
    let mut new_quote = review.quote().clone();
    new_quote.buy_token = dai;
    let new_review = crate::price_swap_review(
        new_plan,
        CowQuote {
            quote: new_quote,
            expiration: String::new(),
            id: None,
            verified: true,
            protocol_fee_bps: None,
        },
        SwapPrice::Unverified,
        U256::from(25),
        U256::from(25),
        50,
        GAS_SHARE_BALANCED_BPS,
        Duration::from_mins(10),
        1,
        U256::ZERO,
        isolation,
    )
    .unwrap();
    // The fixture expires the retry immediately; a real expiry also advances the deadline.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let issue_new_pair = |swap_use| {
        owner.issue_swap_order(crate::SwapOrderSigning {
            review: &new_review,
            swap_use,
            private_minimum: new_review.suggested_private_minimum(),
            price_acknowledged: true,
            transactions: vec![pre_hook_transaction.clone()],
            inputs: std::slice::from_ref(&input),
            change_output_pois: Vec::new(),
            output_pois: &output_pois,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &anchors,
            token_registry: &tokens,
            bridge: None,
            destination_minimum: None,
            destination: None,
            continuing_minimum: None,
        })
    };
    // Another pair is another swap. The retried use refuses it before anything is signed.
    let refused = issue_new_pair(crate::vault::SwapUseId::first(operation))
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.downcast_ref::<crate::vault::ExecutorStoreError>(),
            Some(crate::vault::ExecutorStoreError::SwapUseTermsChanged)
        ),
        "{refused:#}"
    );
    assert_eq!(submissions.lock().unwrap().len(), 3);
    let new_use = crate::vault::SwapUseId::random().unwrap();
    store
        .claim_swap_pair(crate::vault::SwapPairClaim {
            id: new_use,
            source: crate::vault::SwapAccountChoice::Existing(operation),
            delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval: new_review
                .approval(new_review.suggested_private_minimum(), true)
                .unwrap(),
            destination: None,
        })
        .unwrap();
    let SwapOrderOutcome::Submitted { uid: new_uid } = issue_new_pair(new_use).await.unwrap()
    else {
        panic!("new pair submitted");
    };
    assert_eq!(new_uid.owner(), uid.owner());
    {
        let submitted = submissions.lock().unwrap();
        assert_eq!(submitted_order(&submitted[3].1).buyToken, dai);
        assert_eq!(submitted_order(&submitted[0].1).buyToken, USDC);
    }

    assert_eq!(unexpected_reads.load(Ordering::Relaxed), 0);
    orderbook_task.abort();
    fee_server.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
