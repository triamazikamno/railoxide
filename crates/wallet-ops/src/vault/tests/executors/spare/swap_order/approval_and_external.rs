use super::*;

// A new swap is planned, priced, and approved for its reserved executor before any setup is
// paid. The approval outlives the session, and hooks are signed only for a plan made from the
// confirmed delegation.
#[tokio::test]
async fn swap_is_approved_before_setup_but_signed_only_once_delegated() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    let swap_profile = chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let prepared = owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let reserved = crate::SwapExecutor::reserved(&prepared).unwrap();
    // The setup takes the fresh executor's nonce 0, so the pre-hook is planned at 1.
    assert_eq!(
        (
            reserved.operation(),
            reserved.executor(),
            reserved.delegate()
        ),
        (Some(operation), prepared.context().executor, delegate)
    );
    assert_eq!(reserved.expected_pre_hook_nonce(), U256::ONE);
    assert!(reserved.delegated().is_none());

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
        reserved,
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
        panic!("one note fits one order before setup");
    };
    assert_eq!(plan.operation(), Some(operation));
    let quote: CowQuote = serde_json::from_value(json!({
        "quote": {
            "sellToken": WETH, "buyToken": USDC, "sellAmount": "999000", "buyAmount": "3000000000",
            "validTo": 1, "feeAmount": "1000", "gasAmount": "0", "gasPrice": "0",
            "sellTokenPrice": "1000000000000", "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let price_at = |shield_fee: u64, unshield_fee: u64, gas_price_wei: u128| {
        crate::price_swap_review(
            plan.clone(),
            quote.clone(),
            SwapPrice::Unverified,
            U256::from(shield_fee),
            U256::from(unshield_fee),
            50,
            GAS_SHARE_BALANCED_BPS,
            Duration::from_mins(10),
            gas_price_wei,
            U256::ZERO,
            isolation,
        )
        .unwrap()
    };
    let price = |shield_fee, unshield_fee| price_at(shield_fee, unshield_fee, 1_000_000_000);
    let review = price(25, 25);
    // At 1 gwei plus 25% and this quote's rate of at least 3,000 USDC/ETH, price the
    // estimated hook gas. The declared limits' margin only caps execution and isn't priced.
    let estimated_gas = plan.hook_gas_estimate();
    let declared_gas = plan.pre_hook_gas_limit() + plan.post_hook_gas_limit().unwrap();
    assert!(estimated_gas < declared_gas);
    assert!(review.gas_estimate() * U256::from(4) >= U256::from(estimated_gas) * U256::from(15));
    assert!(review.gas_estimate() * U256::from(4) < U256::from(declared_gas) * U256::from(15));
    let private_minimum = review.suggested_private_minimum();
    // An unverified price is approved only with the user's acknowledgement.
    assert!(review.approval(private_minimum, false).is_err());
    let approval = review.approval(private_minimum, true).unwrap();
    // The approval binds the gas share it was priced at, and the uncushioned gas price.
    assert_eq!(
        (
            approval.bounds.gas_share_bps,
            approval.bounds.gas_estimate,
            approval.bounds.gas_allowance,
            approval.bounds.gas_price_wei,
            approval.bounds.valid_for_secs,
        ),
        (
            Some(GAS_SHARE_BALANCED_BPS),
            Some(review.gas_estimate()),
            Some(review.gas_allowance()),
            Some(1_000_000_000),
            Some(600),
        )
    );
    // After setup the entered amount is planned again; the order sells it less the fee.
    assert_eq!(
        (approval.bounds.spend_amount(), approval.bounds.sell_amount),
        (amount, U256::from(997_500))
    );
    owner
        .record_swap_approval(
            operation,
            crate::vault::SwapUseId::first(operation),
            approval.clone(),
        )
        .unwrap();
    let restarted = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let record = restarted
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(record.swap_approval(), Some(&approval));

    // After setup the fresh review keeps the approval unless a fee or the validity moved, or
    // the minimum or the allowed gas moved beyond a fifth of the approved allowed gas.
    assert_eq!(review.approval_change(&approval), None);
    // A setup approval saved before gas shares needs a full review.
    let mut older = approval.clone();
    older.bounds.gas_share_bps = None;
    older.bounds.gas_estimate = None;
    older.bounds.gas_allowance = None;
    older.bounds.gas_price_wei = None;
    older.bounds.valid_for_secs = None;
    assert_eq!(
        review.approval_change(&older),
        Some(SwapReviewChange::GasShare)
    );
    // Gas 10% higher is signed at the approved minimum; 30% higher needs a new review.
    let pricier = price_at(25, 25, 1_100_000_000);
    assert!(pricier.suggested_private_minimum() < private_minimum);
    assert_eq!(
        pricier.approved_order_minimum(&approval),
        Ok(private_minimum)
    );
    let pricier = price_at(25, 25, 1_300_000_000);
    assert_eq!(
        pricier.approval_change(&approval),
        Some(SwapReviewChange::GasAllowance {
            approved: review.gas_allowance(),
            current: pricier.gas_allowance(),
        })
    );
    let mut longer = approval.clone();
    longer.bounds.valid_for_secs = Some(1_800);
    assert_eq!(
        review.approval_change(&longer),
        Some(SwapReviewChange::Validity {
            approved: 1_800,
            current: 600,
        })
    );
    // A lower gas estimate only raises the minimum, so the approval still holds.
    let cheaper = price_at(25, 25, 500_000_000);
    assert!(cheaper.gas_estimate() < review.gas_estimate());
    assert!(cheaper.suggested_private_minimum() > private_minimum);
    assert_eq!(cheaper.approval_change(&approval), None);

    assert_eq!(
        price(30, 25).approval_change(&approval),
        Some(SwapReviewChange::ShieldFee {
            approved: U256::from(25),
            current: U256::from(30),
        })
    );
    assert_eq!(
        price(25, 30).approval_change(&approval),
        Some(SwapReviewChange::UnshieldFee {
            approved: U256::from(25),
            current: U256::from(30),
        })
    );
    // An approved minimum that the quote misses by the whole allowed gas.
    let mut higher = approval.clone();
    higher.bounds.private_minimum = private_minimum + review.gas_allowance();
    assert_eq!(
        review.approval_change(&higher),
        Some(SwapReviewChange::Minimum {
            approved: private_minimum + review.gas_allowance(),
            current: private_minimum,
        })
    );

    // Nothing is signed, persisted, or sent for a plan made before the delegation.
    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook(
        db.clone(),
        view.clone(),
        operation,
        swap_profile.settlement(),
    )
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        orderbook_url,
        1,
    )
    .unwrap();
    let tokens = crate::settings::EffectiveTokenRegistry {
        tokens: std::collections::BTreeMap::new(),
    };
    let executor = prepared.context().executor;
    let authorization = password();
    let output_pois = OutputPois::default();
    let signed = owner
        .issue_swap_order(crate::SwapOrderSigning {
            review: &review,
            swap_use: crate::vault::SwapUseId::first(review.plan().operation().unwrap()),
            private_minimum,
            price_acknowledged: true,
            transactions: vec![Transaction {
                proof: SnarkProof::default(),
                merkleRoot: B256::ZERO,
                nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
                commitments: vec![B256::ZERO],
                boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
                unshieldPreimage: CommitmentPreimage::empty(),
            }],
            inputs: std::slice::from_ref(&input),
            change_output_pois: Vec::new(),
            output_pois: &output_pois,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &crate::TokenAnchorRateCache::new(),
            token_registry: &tokens,
            bridge: None,
            destination_minimum: None,
            destination: None,
            continuing_minimum: None,
        })
        .await;
    assert!(signed.is_err());
    let record = restarted
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert!(record.swap().is_none() && record.issued().is_empty());
    assert!(submissions.lock().unwrap().is_empty());

    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(restarted);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// An External order pays its approved receiver and carries only the pre-hook. The setup's
// approval, with its pair and delivery, binds the first order; later orders on the account may
// choose either delivery kind.
#[tokio::test]
async fn external_order_pays_the_approved_receiver_without_a_post_hook() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let profile = chain.accepted_executor_profile().unwrap();
    let delegate = profile.delegate();
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
    let approved = SwapDelivery::External {
        receiver: Address::repeat_byte(0x91),
    };
    let receiver = Address::repeat_byte(0x92);
    let moved = SwapDelivery::External { receiver };

    // The approved terms are in the write that creates the record, before its address is bound.
    let reserved = ExecutorOperationId::random().unwrap();
    store
        .reserve_with_swap_approval(
            reserved,
            delegate,
            Some("Private swap"),
            &[crate::ExecutorAsset::Erc20(USDC)],
            Some(setup_approval(USDC, Address::ZERO, approved.clone())),
            None,
        )
        .unwrap();
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == reserved)
        .unwrap();
    assert_eq!(record.swap_approval_tokens(), Some((USDC, Address::ZERO)));
    assert_eq!(
        record.swap_approval().map(|approval| &approval.delivery),
        Some(&approved)
    );

    let operation = ExecutorOperationId::random().unwrap();
    let executor = owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(USDC, Address::ZERO, approved.clone()),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .context()
        .executor;
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

    let amount = U256::from(1_000_000);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            USDC,
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
    let quote: CowQuote = serde_json::from_value(json!({
        "quote": {
            "sellToken": USDC, "buyToken": BUY_NATIVE_TOKEN, "sellAmount": "997500",
            "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0", "gasAmount": "0",
            "gasPrice": "0", "sellTokenPrice": "1000000000000", "kind": "sell",
            "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let review_at = |buy_token, delivery: &SwapDelivery, gas_price_wei: u128| {
        let SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
            &builder,
            &swap_profile,
            delegated,
            std::slice::from_ref(&input),
            &SwapAmountRequest {
                sell_token: USDC,
                buy_token,
                amount,
                delivery: delivery.clone(),
                byte_budget: None,
            },
            swap_profile.app_data_byte_budget(),
            None,
        )
        .unwrap() else {
            panic!("one note fits one order");
        };
        let mut review = crate::price_swap_review(
            plan,
            quote.clone(),
            SwapPrice::Unverified,
            U256::from(25),
            U256::from(25),
            50,
            GAS_SHARE_BALANCED_BPS,
            Duration::from_mins(10),
            gas_price_wei,
            U256::ZERO,
            isolation,
        )
        .unwrap();
        // A Bridge review carries its bridge quote, whose minimum the approval binds.
        if let SwapDelivery::Bridge(bridge) = delivery {
            review.set_bridge_for_tests(&crate::SwapBridgeQuote {
                provider: bridge.provider,
                destination_minimum: U256::from(1_000),
                expected_output: U256::from(1_010),
                expected_output_usd: None,
                fee: Some(U256::ZERO),
                leg: crate::BridgeLegPrice::SameAsset,
                fill_time_sec: None,
                private: None,
            });
        }
        review
    };
    let review = |buy_token, delivery: SwapDelivery| review_at(buy_token, &delivery, 1);
    let first = review(Address::ZERO, approved.clone());
    let changed = review(Address::ZERO, moved.clone());

    // The receiver's address is part of the approved terms; its label is not, so the same
    // address keeps the approval.
    let approval = first
        .approval(first.suggested_private_minimum(), true)
        .unwrap();
    assert_eq!(first.approval_change(&approval), None);
    assert_eq!(
        changed.approval_change(&approval),
        Some(SwapReviewChange::Delivery)
    );
    let mut private = approval.clone();
    private.delivery = SwapDelivery::Reshield;
    assert_eq!(
        first.approval_change(&private),
        Some(SwapReviewChange::Delivery)
    );
    // No post-hook, no shield fee: the receiver's minimum is the buy amount.
    assert_eq!(approval.bounds.post_hook_gas_limit, None);
    assert_eq!(approval.bounds.shield_fee_bps, U256::ZERO);
    assert_eq!(approval.bounds.buy_amount, approval.bounds.private_minimum);

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
    let transaction = Transaction {
        proof: SnarkProof::default(),
        merkleRoot: B256::ZERO,
        nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
        commitments: vec![B256::ZERO],
        boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
        unshieldPreimage: CommitmentPreimage::empty(),
    };
    let authorization = password();
    let output_pois = OutputPois::default();
    let anchors = crate::TokenAnchorRateCache::new();
    let tokens = crate::settings::EffectiveTokenRegistry {
        tokens: std::collections::BTreeMap::new(),
    };
    macro_rules! issue {
        ($review:expr) => {
            issue!($review, None)
        };
        ($review:expr, $bridge:expr) => {
            issue!($review, $bridge, $review.suggested_private_minimum())
        };
        ($review:expr, $bridge:expr, $private_minimum:expr) => {
            owner
                .issue_swap_order(crate::SwapOrderSigning {
                    review: $review,
                    swap_use: crate::vault::SwapUseId::first($review.plan().operation().unwrap()),
                    private_minimum: $private_minimum,
                    price_acknowledged: true,
                    transactions: vec![transaction.clone()],
                    inputs: std::slice::from_ref(&input),
                    change_output_pois: Vec::new(),
                    output_pois: &output_pois,
                    authorization: &authorization,
                    orderbook: &orderbook,
                    anchor_cache: &anchors,
                    token_registry: &tokens,
                    bridge: $bridge,
                    destination_minimum: $review.bridge().map(|bridge| bridge.destination_minimum),
                    destination: None,
                    continuing_minimum: None,
                })
                .await
        };
    }
    let record = || {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };

    // The first order must be the one approved with the setup. Another receiver returns to
    // review and another pair is refused, both before anything is signed.
    assert_eq!(
        issue!(&changed).unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::Delivery)
    );
    let dai = alloy::primitives::address!("6b175474e89094c44da98b954eedeac495271d0f");
    let other_pair = review(dai, approved.clone());
    assert!(
        issue!(&other_pair)
            .unwrap_err()
            .to_string()
            .contains("differ from the ones approved")
    );
    let unsigned = record();
    assert!(unsigned.swap().is_none());
    assert_eq!(unsigned.issued().len(), 1);
    assert_eq!(
        unsigned.swap_approval().map(|approval| &approval.delivery),
        Some(&approved)
    );
    assert!(submissions.lock().unwrap().is_empty());

    // A Bridge approval binds each destination term: a review that differs in any of them
    // needs a full review.
    let bridged = BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 137,
        receiver: Address::repeat_byte(0x93),
        destination_token: Address::repeat_byte(0x94),
        surplus: BridgeSurplus::BridgedByProvider,
        private: None,
    };
    let bridge = review(WETH, SwapDelivery::Bridge(bridged));
    let bridge_approval = bridge
        .approval(bridge.suggested_private_minimum(), true)
        .unwrap();
    assert_eq!(bridge.approval_change(&bridge_approval), None);
    for other_terms in [
        BridgeDelivery {
            provider: BridgeProvider::Across,
            ..bridged
        },
        BridgeDelivery {
            destination_chain: 56,
            ..bridged
        },
        BridgeDelivery {
            destination_token: Address::ZERO,
            ..bridged
        },
        BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..bridged
        },
    ] {
        let mut other = bridge_approval.clone();
        other.delivery = SwapDelivery::Bridge(other_terms);
        assert_eq!(
            bridge.approval_change(&other),
            Some(SwapReviewChange::Delivery)
        );
    }
    // NEAR Intents converts the whole deposit, so it can't reshield surplus.
    let reshielding = review(
        WETH,
        SwapDelivery::Bridge(BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..bridged
        }),
    );
    assert!(
        reshielding
            .approval(reshielding.suggested_private_minimum(), true)
            .is_err()
    );

    // Each protocol contract on the destination chain is refused before anything is signed,
    // even once approved, and so is an order without its bridge route.
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let polygon = chains.get(137).unwrap();
    let clients = owner.swap_bridge_clients(&orderbook).unwrap();
    let near_destination = crate::bridge::BridgeDestination {
        destination_token: bridged.destination_token,
        intermediate: WETH,
        symbol: "DST".to_owned(),
        same_asset: false,
        near: Some(crate::bridge::NearAssets {
            origin_asset: "nep141:weth".to_owned(),
            destination_asset: "nep141:dst".to_owned(),
            origin_decimals: 18,
            destination_decimals: 18,
        }),
    };
    let route = crate::SwapBridgeRoute {
        clients: &clients,
        destination: crate::SwapBridgeDestination::Chain {
            destination: &near_destination,
            chain: polygon,
        },
    };
    let polygon_railgun = polygon.require_railgun().unwrap().deployment.contract;
    let spoke_pool = polygon.bridge_profile().unwrap().spoke_pool();
    for (receiver, rejection) in [
        (Address::ZERO, BridgeReceiverRejection::ZeroAddress),
        (polygon_railgun, BridgeReceiverRejection::Railgun),
        (spoke_pool, BridgeReceiverRejection::SpokePool),
    ] {
        let refused = review(
            WETH,
            SwapDelivery::Bridge(BridgeDelivery {
                receiver,
                ..bridged
            }),
        );
        owner
            .record_swap_approval(
                operation,
                crate::vault::SwapUseId::first(operation),
                refused
                    .approval(refused.suggested_private_minimum(), true)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            issue!(&refused, Some(route))
                .unwrap_err()
                .downcast_ref::<BridgeReceiverRejection>(),
            Some(&rejection)
        );
    }
    // A quote carries the zero address until a receiver is entered. An External order to it is
    // refused as well.
    let unaddressed = review(
        Address::ZERO,
        SwapDelivery::External {
            receiver: Address::ZERO,
        },
    );
    owner
        .record_swap_approval(
            operation,
            crate::vault::SwapUseId::first(operation),
            unaddressed
                .approval(unaddressed.suggested_private_minimum(), true)
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        issue!(&unaddressed)
            .unwrap_err()
            .downcast_ref::<SwapReceiverRejection>(),
        Some(&SwapReceiverRejection::ZeroAddress)
    );
    owner
        .record_swap_approval(
            operation,
            crate::vault::SwapUseId::first(operation),
            bridge_approval,
        )
        .unwrap();
    assert!(issue!(&bridge).is_err());
    assert!(record().swap().is_none());
    assert_eq!(record().issued().len(), 1);
    assert!(submissions.lock().unwrap().is_empty());

    // The user approves the new receiver at a gas estimate of an eighth of the quote. The
    // requote after setup finds gas 10% higher, within a fifth of the approved allowed gas, so
    // the order is signed at the approved minimum and persisted. Its submission response is
    // lost, and the resubmission rebuilds the identical order.
    let gas_price_wei = 100_000_000_000_000_000 / u128::from(changed.plan().hook_gas_estimate());
    let reviewed = review_at(Address::ZERO, &moved, gas_price_wei);
    let approval = reviewed
        .approval(reviewed.suggested_private_minimum(), true)
        .unwrap();
    let private_minimum = approval.bounds.private_minimum;
    let requote = review_at(Address::ZERO, &moved, gas_price_wei + gas_price_wei / 10);
    assert!(requote.suggested_private_minimum() < private_minimum);
    assert_eq!(
        requote.approved_order_minimum(&approval),
        Ok(private_minimum)
    );
    owner
        .record_swap_approval(
            operation,
            crate::vault::SwapUseId::first(operation),
            approval,
        )
        .unwrap();
    assert!(
        issue!(&requote, None, private_minimum)
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let signed = record();
    let saved = signed.swap().unwrap().orders()[0].clone();
    assert_eq!(saved.delivery(), moved);
    assert!(saved.post_hook().is_none());
    // The attempt records the gas its minimum leaves room for, less than the requote's own
    // allowed gas. The setup approval keeps the gas it was approved with.
    let room = requote.gas_allowance_for(private_minimum).unwrap();
    assert!(room < requote.gas_allowance());
    assert_eq!(saved.bounds().gas_allowance, Some(room));
    assert_eq!(
        signed.swap_approval().unwrap().bounds.gas_allowance,
        Some(reviewed.gas_allowance())
    );
    let [_, pre_hook] = signed.issued() else {
        panic!("an External order issues only its pre-hook");
    };
    assert_eq!(pre_hook.purpose(), ExecutorPayloadPurpose::SwapPreHook);
    // The executor never holds the bought asset, so recovery doesn't track it.
    assert_eq!(signed.assets(), &[crate::ExecutorAsset::Erc20(USDC)]);
    assert_eq!(
        owner
            .resubmit_swap_order(operation, &orderbook)
            .await
            .unwrap(),
        SwapOrderOutcome::Submitted { uid: saved.uid() }
    );
    let submitted = submissions.lock().unwrap().clone();
    let [(persisted, body), (_, resent)] = submitted.as_slice() else {
        panic!("one initial request and one resubmission");
    };
    assert!(*persisted);
    assert_eq!(body, resent);
    let order = submitted_order(body);
    assert_eq!(
        (order.receiver, order.buyToken, order.buyAmount),
        (receiver, BUY_NATIVE_TOKEN, private_minimum)
    );
    assert_eq!(order.buyTokenBalance, TOKEN_BALANCE_ERC20);
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert_eq!((hooks.pre.len(), hooks.post.len()), (1, 0));

    // After that order ends, the account can place a Reshield order for another pair.
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
        .record_swap_observations(operation, saved.uid(), expired)
        .unwrap();
    // The fixture expires the order immediately; a real expiry also advances the deadline.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let reshield = review(WETH, SwapDelivery::Reshield);
    // Another pair and delivery is another swap, which claims the account for its own use.
    let new_use = crate::vault::SwapUseId::random().unwrap();
    store
        .claim_swap_pair(crate::vault::SwapPairClaim {
            id: new_use,
            source: crate::vault::SwapAccountChoice::Existing(operation),
            delegate: record().delegate(),
            purpose_summary: None,
            assets: Vec::new(),
            approval: reshield
                .approval(reshield.suggested_private_minimum(), true)
                .unwrap(),
            destination: None,
        })
        .unwrap();
    assert!(matches!(
        owner
            .issue_swap_order(crate::SwapOrderSigning {
                review: &reshield,
                swap_use: new_use,
                private_minimum: reshield.suggested_private_minimum(),
                price_acknowledged: true,
                transactions: vec![transaction.clone()],
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
            .await
            .unwrap(),
        SwapOrderOutcome::Submitted { .. }
    ));
    let body = submissions.lock().unwrap()[2].1.clone();
    assert_eq!(submitted_order(&body).receiver, executor);
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert_eq!(hooks.post.len(), 1);
    assert_eq!(
        record().assets(),
        &[
            crate::ExecutorAsset::Erc20(USDC),
            crate::ExecutorAsset::Erc20(WETH)
        ]
    );

    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
