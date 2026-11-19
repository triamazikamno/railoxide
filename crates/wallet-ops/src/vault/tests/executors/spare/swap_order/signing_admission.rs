use super::*;

// The swap's own setup is in the latest block and in no confirmed one. The order is signed
// after it, at the next nonce, on a read at the latest block made again under the write's lock,
// and so is its speed-up. The setup can be sent again beside the open order, and a confirmed
// read of either setup leaves the order's pre-hook at the account's nonce.
#[tokio::test]
async fn an_order_on_an_included_setup_signs_after_it_on_a_fresh_latest_read() {
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
    let setup_owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let prepared = setup_owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(USDC, WETH, SwapDelivery::Reshield),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let executor = prepared.context().executor;
    let note = |amount: u64, position: u64, random: u8| {
        Utxo::new(
            broadcaster_core::notes::Note::new_change(
                view.scan_keys().master_public_key,
                USDC,
                U256::from(amount),
                [random; 16],
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
    let nullifier = |note: &Utxo| B256::from(note.nullifier(view.scan_keys().nullifying_key));
    let positions = |notes: Vec<Utxo>| notes.iter().map(|note| note.position).collect::<Vec<_>>();
    // Each note is of the sell token, so the setup's fee and the order could share one.
    let amount = 1_000_000;
    let (input, setup_fee, retry_fee) = (note(amount, 0, 7), note(9, 1, 6), note(9, 2, 5));
    // A fee-only setup at the account's first nonce.
    let setup_call = |fee: &Utxo| railgun_wallet::TransactionCall {
        to: executor,
        data: RelayAdapt7702::executeCall {
            _transactions: vec![Transaction {
                proof: SnarkProof::default(),
                merkleRoot: B256::ZERO,
                nullifiers: vec![nullifier(fee)],
                commitments: Vec::new(),
                boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
                unshieldPreimage: CommitmentPreimage::empty(),
            }],
            _actionData: RelayAdapt7702ActionData {
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls: Vec::new(),
            },
            _nonce: U256::ZERO,
            _signature: Bytes::new(),
        }
        .abi_encode()
        .into(),
    };
    setup_owner
        .issue_operation(
            &prepared,
            &setup_call(&setup_fee),
            std::slice::from_ref(&setup_fee),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    setup_owner.shutdown().await;
    drop(setup_owner);

    // The setup consumed nonce 0 in the latest block 14. The confirmed block 13 doesn't show
    // it. A restarted owner reacquires the evidence.
    rpc.state.head.store(14, Ordering::Relaxed);
    rpc.set_head_account(executor, Some((delegate, U256::ONE)));
    let owner = ExecutorOwner::new(
        1,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let record_of = || {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };
    let first_use = crate::vault::SwapUseId::first(operation);
    let refreshed = owner
        .reuse_swap_account(
            operation,
            13,
            SwapAccountRole::Source,
            SwapAccountUse::Claimed(first_use),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let delegated = refreshed.delegated().unwrap();
    let unplaced = record_of();
    let observed = unplaced.nonce_observation().unwrap();
    assert_eq!(
        (observed.block().number, observed.nonce()),
        (13, U256::ZERO)
    );
    assert_eq!(delegated.observed(), observed);
    assert_eq!(
        delegated
            .inclusion()
            .map(|inclusion| (inclusion.block(), inclusion.nonce())),
        Some((14, U256::ONE))
    );
    assert_eq!(refreshed.expected_pre_hook_nonce(), U256::ONE);
    // The order doesn't spend the note the setup may still spend as its fee.
    assert_eq!(
        positions(
            owner
                .inputs_for_swap_order(vec![setup_fee.clone(), input.clone()], &unplaced)
                .unwrap()
        ),
        [input.position]
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
            sell_token: USDC,
            buy_token: WETH,
            amount: U256::from(amount),
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
    let transaction = Transaction {
        proof: SnarkProof::default(),
        merkleRoot: B256::ZERO,
        nullifiers: vec![nullifier(&input)],
        commitments: vec![B256::ZERO],
        boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
        unshieldPreimage: CommitmentPreimage::empty(),
    };
    let review = review_of(plan, GAS_SHARE_BALANCED_BPS);
    let minimum = review.suggested_private_minimum();

    // A reorg removed the setup after the quote was checked. Nothing is signed or sent.
    rpc.set_head_account(executor, None);
    let refused = sign_order(
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
        refused.downcast_ref::<SwapAccountRefusal>(),
        Some(&SwapAccountRefusal::SetupUnconfirmed),
        "{refused:#}"
    );
    assert_eq!(record_of(), unplaced);
    assert!(submissions.lock().unwrap().is_empty());

    // The setup is included again. The pre-hook takes nonce 1 and the post-hook nonce 2, both
    // naming the confirmed observation, which stays the record's.
    rpc.set_head_account(executor, Some((delegate, U256::ONE)));
    let SwapOrderOutcome::Submitted { uid: first } = sign_order(
        &owner,
        &orderbook,
        &review,
        minimum,
        &transaction,
        &input,
        None,
    )
    .await
    .unwrap() else {
        panic!("the order is submitted");
    };
    let placed = record_of();
    assert_eq!(
        (placed.nonce_observation(), placed.nonce_watermark()),
        (Some(observed), unplaced.nonce_watermark())
    );
    let hooks = placed
        .issued()
        .iter()
        .filter(|payload| payload.purpose() != ExecutorPayloadPurpose::Operation)
        .map(|payload| {
            let calldata = payload.context().calldata();
            let signed = match payload.purpose() {
                ExecutorPayloadPurpose::SwapPreHook => {
                    RelayAdapt7702::executeCall::abi_decode(calldata)
                        .unwrap()
                        ._nonce
                }
                _ => {
                    RelayAdapt7702::multicallCall::abi_decode(calldata)
                        .unwrap()
                        ._nonce
                }
            };
            (
                payload.purpose(),
                payload.nonce(),
                signed,
                payload.context().observed(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        hooks,
        [
            (
                ExecutorPayloadPurpose::SwapPreHook,
                U256::ONE,
                U256::ONE,
                observed
            ),
            (
                ExecutorPayloadPurpose::SwapPostHook,
                U256::from(2),
                U256::from(2),
                observed
            ),
        ]
    );

    // A speed-up is offered and signed at the open order's nonce only on the latest read.
    assert_eq!(
        owner.swap_speed_up_refusal(operation, first, None).unwrap(),
        Some(crate::SwapSpeedUpRefusal::Unavailable)
    );
    assert_eq!(
        owner
            .swap_speed_up_refusal(operation, first, delegated.inclusion())
            .unwrap(),
        None
    );
    let plan = speed_up_plan(
        &store,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        first,
    )
    .unwrap();
    let speed_up = review_of(plan, GAS_SHARE_LOOSE_BPS);
    let speed_up_minimum = speed_up.suggested_private_minimum();
    rpc.set_head_account(executor, None);
    assert_eq!(
        sign_order(
            &owner,
            &orderbook,
            &speed_up,
            speed_up_minimum,
            &transaction,
            &input,
            None,
        )
        .await
        .unwrap_err()
        .downcast_ref::<SwapAccountRefusal>(),
        Some(&SwapAccountRefusal::SetupUnconfirmed)
    );
    assert_eq!(record_of(), placed);
    rpc.set_head_account(executor, Some((delegate, U256::ONE)));
    let SwapOrderOutcome::Submitted { uid: second } = sign_order(
        &owner,
        &orderbook,
        &speed_up,
        speed_up_minimum,
        &transaction,
        &input,
        None,
    )
    .await
    .unwrap() else {
        panic!("the speed-up is submitted");
    };
    let sped_up = record_of();
    let orders = sped_up.swap().unwrap().orders();
    assert_eq!(
        orders
            .iter()
            .map(|order| (order.uid(), order.pre_hook().nonce()))
            .collect::<Vec<_>>(),
        [(first, U256::ONE), (second, U256::ONE)]
    );

    // The setup left the latest block. It is sent again at its nonce next to the open order,
    // with a fee note that order's pre-hook doesn't spend.
    rpc.set_head_account(executor, None);
    assert!(owner.swap_setup_preview(operation).is_ok());
    let retry = owner
        .resume_swap_setup(
            operation,
            broadcaster(delegate),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(retry.context().execution_nonce, U256::ZERO);
    let notes = vec![setup_fee.clone(), retry_fee.clone(), input.clone()];
    assert_eq!(
        positions(owner.inputs_for_preparation(notes.clone(), &retry).unwrap()),
        [setup_fee.position, retry_fee.position]
    );
    owner
        .issue_operation(
            &retry,
            &setup_call(&retry_fee),
            std::slice::from_ref(&retry_fee),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let retried = record_of();
    let setups = retried
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
        .map(crate::vault::IssuedExecutorPayload::hash)
        .collect::<Vec<_>>();
    assert_eq!(setups.len(), 2);
    // An order excludes the notes of every setup attempt that can still run.
    assert_eq!(
        positions(owner.inputs_for_swap_order(notes, &retried).unwrap()),
        [input.position]
    );

    // Either setup runs. One confirmed read resolves the setup, and the orders' pre-hooks stay
    // pending at the account's nonce.
    rpc.state.head.store(15, Ordering::Relaxed);
    rpc.set_delegated_account(executor, delegate, U256::ONE);
    let checked = owner
        .observe_swap_setup(operation, crate::OperationHttpClient::direct_for_tests())
        .await
        .unwrap();
    assert!(matches!(checked.status, SwapSetupStatus::Delegated(_)));
    assert_eq!(checked.inclusion, None);
    let resolved = record_of();
    assert_eq!(resolved.nonce_observation().unwrap().nonce(), U256::ONE);
    for setup in setups {
        assert_eq!(
            resolved.payload_state(setup),
            Some(ExecutorPayloadState::Resolved)
        );
    }
    for order in resolved.swap().unwrap().orders() {
        assert_eq!(
            resolved.payload_state(order.pre_hook().payload()),
            Some(ExecutorPayloadState::Pending)
        );
    }

    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// A speed-up of an Across order qualifies by its final `CoW` buy amount, not by its
// destination minimum. The comparison is made again after a signing-time quote raised the
// deposit within the cushion, and the raised deposit is what gets signed and recorded.
#[tokio::test]
async fn an_across_speed_up_qualifies_by_its_final_buy_amount() {
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
    let delivery = BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver: Address::repeat_byte(0x93),
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

    // Across quotes 1,005 for a deposit of at least the first amount, and the second amount
    // for a smaller one.
    let quoted = Arc::new(Mutex::new((U256::ZERO, 999_u64)));
    let answer = quoted.clone();
    let (across_url, across_requests, across_task) = spawn_bridge_stub_with(move |path, _| {
        let deposit = path
            .split(['?', '&'])
            .find_map(|pair| pair.strip_prefix("amount="))
            .map_or(U256::ZERO, |amount| amount.parse().unwrap());
        let (full_from, short) = *answer.lock().unwrap();
        let output = if deposit >= full_from { 1_005 } else { short };
        (
            200,
            across_fee_quote(spoke_pool, U256::from(output), 3 * 60 * 60),
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
    // `plan`'s review with its hooks' gas priced at `gas_wei` in total. At 0.1 native tokens
    // the allowed gas is about 3% of the quote, as in the cushion case of the order test
    // above, and each further 0.1 lowers the minimum by about as much again.
    let gas_wei = 100_000_000_000_000_000_u128;
    let priced = |plan: &crate::SwapInputPlan, gas_wei: u128| {
        BridgeOrderFixture::review_at(
            plan.clone(),
            &swap_profile,
            delivery.provider,
            gas_wei / u128::from(plan.hook_gas_estimate()),
            1_000,
        )
    };
    let bridged = |destination_minimum: u64| Some((route, U256::from(destination_minimum), None));
    let quotes = || across_requests.lock().unwrap().len();
    let last_quoted = || across_requests.lock().unwrap().last().cloned().unwrap().0;

    let first_review = priced(fixture.review.plan(), gas_wei);
    let first_buy = first_review.suggested_private_minimum();
    let SwapOrderOutcome::Submitted { uid: first } = sign_order(
        &owner,
        &orderbook,
        &first_review,
        first_buy,
        &fixture.transaction,
        &fixture.input,
        bridged(1_000),
    )
    .await
    .unwrap() else {
        panic!("the first order is submitted");
    };
    let unsigned = fixture.record(&store);
    let delegated = fixture.review.plan().swap_executor().delegated().unwrap();
    let notes = std::slice::from_ref(&fixture.input);
    let plan = speed_up_plan(&store, &swap_profile, delegated, notes, first).unwrap();

    // A higher bridge fee lowers the destination minimum to 990 while the order still asks for
    // the open order's buy amount. That relaxes nothing, and Across isn't even asked.
    let same_cost = priced(&plan, gas_wei);
    let requested = quotes();
    assert_eq!(
        sign_order(
            &owner,
            &orderbook,
            &same_cost,
            first_buy,
            &fixture.transaction,
            &fixture.input,
            bridged(990),
        )
        .await
        .unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::SpeedUpBuyAmount {
            open: first_buy,
            current: first_buy,
        })
    );
    assert_eq!(quotes(), requested);

    // The order asks for one unit less, but its deposit is quoted 999. Raising the deposit to
    // deliver 1,000 again stays within the cushion and takes the buy amount back above the
    // open order's, so the raised order is no speed-up.
    *quoted.lock().unwrap() = (first_buy, 999);
    let outcome = sign_order(
        &owner,
        &orderbook,
        &same_cost,
        first_buy - U256::ONE,
        &fixture.transaction,
        &fixture.input,
        bridged(1_000),
    )
    .await
    .unwrap();
    let SwapOrderOutcome::ReviewRequired(SwapReviewChange::SpeedUpBuyAmount { open, current }) =
        outcome
    else {
        panic!("the raised deposit isn't below the open order's buy amount: {outcome:?}");
    };
    assert!(open == first_buy && current >= first_buy);
    // The refusal compares the deposit Across quoted last, the raised one.
    assert_eq!(quotes(), requested + 2);
    let path = last_quoted();
    assert!(path.contains(&format!("amount={current}")), "{path}");

    // A quote of 900 can't be made up within the cushion, so the destination minimum needs a
    // new review even though the order asks for less.
    let lower = priced(&plan, 2 * gas_wei);
    let lower_buy = lower.suggested_private_minimum();
    assert!(lower_buy < first_buy);
    *quoted.lock().unwrap() = (U256::MAX, 900);
    assert_eq!(
        sign_order(
            &owner,
            &orderbook,
            &lower,
            lower_buy,
            &fixture.transaction,
            &fixture.input,
            bridged(1_000),
        )
        .await
        .unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(900),
        })
    );
    // None of these signed, persisted or sent anything.
    assert_eq!(fixture.record(&store), unsigned);
    assert_eq!(submissions.lock().unwrap().len(), 1);

    // A lower buy amount with the same destination minimum of 1,000 is a speed-up.
    *quoted.lock().unwrap() = (U256::ZERO, 999);
    let SwapOrderOutcome::Submitted { uid: second } = sign_order(
        &owner,
        &orderbook,
        &lower,
        lower_buy,
        &fixture.transaction,
        &fixture.input,
        bridged(1_000),
    )
    .await
    .unwrap() else {
        panic!("the speed-up is submitted");
    };
    let record = fixture.record(&store);
    let [kept, replacement] = record.swap().unwrap().orders() else {
        panic!("the speed-up is the second order");
    };
    assert_eq!(kept, &unsigned.swap().unwrap().orders()[0]);
    assert_eq!(
        (replacement.uid(), replacement.invalidates()),
        (second, Some(first))
    );
    assert_eq!(replacement.pre_hook().nonce(), kept.pre_hook().nonce());
    assert_eq!(replacement.bounds().buy_amount, lower_buy);
    assert_eq!(
        replacement.bounds().destination_minimum,
        kept.bounds().destination_minimum
    );

    // With the first order's expiry final, the second is the open one. Its speed-up is quoted
    // 999 for its deposit. The raise that delivers 1,000 again still leaves the buy amount
    // below the open order's, so the raised order is signed.
    store
        .record_swap_observations(
            fixture.operation,
            first,
            SwapOrderObservations {
                expired: Some(SwapObservation {
                    block: record.nonce_observation().unwrap().block(),
                    transaction_hash: None,
                }),
                ..SwapOrderObservations::default()
            },
        )
        .unwrap();
    let plan = speed_up_plan(&store, &swap_profile, delegated, notes, second).unwrap();
    let lowest = priced(&plan, 3 * gas_wei);
    let lowest_buy = lowest.suggested_private_minimum();
    *quoted.lock().unwrap() = (lowest_buy + U256::ONE, 999);
    let SwapOrderOutcome::Submitted { uid: third } = sign_order(
        &owner,
        &orderbook,
        &lowest,
        lowest_buy,
        &fixture.transaction,
        &fixture.input,
        bridged(1_000),
    )
    .await
    .unwrap() else {
        panic!("the raised speed-up is submitted");
    };
    let record = fixture.record(&store);
    let raised = record.swap().unwrap().orders().last().unwrap();
    assert_eq!(raised.uid(), third);
    let signed_buy = raised.bounds().buy_amount;
    assert!(lowest_buy < signed_buy && signed_buy < lower_buy);
    // The record, the order and the post-hook's deposit all carry the raised deposit that
    // Across quoted last, for the approved destination minimum.
    let Some(BridgeOrderTerms::Across(terms)) = raised.bridge().cloned() else {
        panic!("an Across order keeps its deposit terms");
    };
    assert_eq!(
        (terms.input_amount, terms.output_amount),
        (signed_buy, U256::from(1_000))
    );
    assert_eq!(
        (
            raised.bounds().private_minimum,
            raised.bounds().destination_minimum
        ),
        (signed_buy, Some(U256::from(1_000)))
    );
    let path = last_quoted();
    assert!(path.contains(&format!("amount={signed_buy}")), "{path}");
    let body = submissions.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(submitted_order(&body).buyAmount, signed_buy);
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    let post_hook = RelayAdapt7702::multicallCall::abi_decode(&hooks.post[0].call_data).unwrap();
    let deposit = SpokePool::depositV3Call::abi_decode(&post_hook._calls[2].data).unwrap();
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (signed_buy, U256::from(1_000))
    );

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
