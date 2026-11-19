use super::*;

/// Deploy `SwapMath` on `fork` through the deterministic deployer, as its release does, unless
/// the forked chain already has it.
async fn deploy_swap_math(fork: &ForkChain) {
    if fork.code(SWAP_MATH_ADDRESS).await.is_empty() {
        let receipt = fork
            .send_as_bypass(
                DETERMINISTIC_DEPLOYER,
                swap_math_deployment_calldata(),
                1_000_000,
            )
            .await;
        assert!(
            receipt.status(),
            "the deterministic deployer deploys the math contract"
        );
    }
    assert_eq!(
        alloy::primitives::keccak256(fork.code(SWAP_MATH_ADDRESS).await),
        SWAP_MATH_RUNTIME_CODE_HASH
    );
}

/// Place an order of the Public account `signer` through the wallet: it sells WETH for USDC
/// paid to the account's cow-shed proxy, whose hook batch deposits the proxy's whole balance
/// for a new destination stealth account on `route`'s chain. The account approves the vault
/// relayer, the delivery is signed for the order's buy amount and `validTo` against a quote
/// dated `quote_ahead` seconds ahead of the swap chain's clock, and the order is taken from
/// what the orderbook stub received.
async fn place_public_order(
    wallet: &Wallet,
    fork: &ForkChain,
    destination_fork: &ForkChain,
    signer: &VaultedPublicSigner,
    route: OrderDestination,
    quote_ahead: u64,
) -> PlacedOrder {
    let origin = &wallet.chain;
    let owner = &wallet.destination.as_ref().unwrap().owner;
    let profile = origin.public_swap_profile().unwrap();
    let source = signer.address();
    let proxy = proxy_address(
        profile.cow_shed_factory(),
        profile.cow_shed_implementation(),
        source,
    );
    let sell_amount = U256::from(SELL_AMOUNT);
    // The account holds gas money and the WETH it sells.
    fork.impersonate(source).await;
    fork.add_erc20(WETH, source, sell_amount).await;
    // The limit a review declares for the hook: an account's first batch deploys its proxy.
    let proxy_deployed = !fork.code(proxy).await.is_empty();
    let hook_gas_limit = crate::cow::hook_gas_limit(crate::cow::public_deposit_hook_gas(
        proxy_deployed,
        railgun_wallet::tx::GasEstimateMode::UpperBound,
    ));
    let swap = wallet
        .public_swap(
            destination_fork,
            signer,
            PublicTerms {
                sell_token: WETH,
                sell_amount,
                bridged_token: USDC,
                bridged_amount: U256::from(PUBLIC_ORDER_BUY_AMOUNT),
                destination_token: route.token,
                destination_minimum: U256::from(route.minimum),
                hook_gas_limit: Some(hook_gas_limit),
            },
        )
        .await;
    owner
        .submit_public_swap_approvals_with_signer(
            swap.operation,
            swap.id,
            origin,
            signer,
            PUBLIC_GAS_FEE,
            crate::OperationHttpClient::direct_for_tests(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(
        fork.call(
            WETH,
            ForkAllowance::allowanceCall {
                owner: source,
                spender: profile.vault_relayer(),
            },
        )
        .await,
        sell_amount
    );

    // A new block puts the fork's clock at the present before the quote is dated.
    fork.mine(1).await;
    let quoted = fork.timestamp().await + quote_ahead;
    let valid_to = valid_until(PUBLIC_VALID_SECS);
    let signed = wallet.sign_public_delivery(&swap, quoted, valid_to).await;
    let orderbook = spawn_public_orderbook(origin.chain_id, profile.settlement()).await;
    let outcome = owner
        .submit_public_swap_order_with_signer(
            swap.operation,
            swap.id,
            origin,
            signer,
            &orderbook.client,
            &signed.delivery,
            &signed.terms,
            valid_to,
            new_public_swap_batch_nonce().unwrap(),
            None,
            false,
        )
        .await
        .unwrap();
    let PublicSwapOrderOutcome::Submitted { uid } = outcome else {
        panic!("the order is submitted: {outcome:?}");
    };
    let body = orderbook.orders.lock().unwrap().last().cloned().unwrap();
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert_eq!(hooks.pre, [] as [AppDataHook; 0]);
    let [hook] = hooks.post.as_slice() else {
        panic!("the order has one post-hook");
    };
    assert_eq!(
        (hook.target, hook.gas_limit),
        (profile.cow_shed_factory(), hook_gas_limit)
    );
    let order = submitted_order(&body);
    assert_eq!(
        (order.sellToken, order.buyToken, order.receiver),
        (WETH, USDC, proxy)
    );
    assert_eq!(
        (order.sellAmount, order.buyAmount, order.validTo),
        (sell_amount, U256::from(PUBLIC_ORDER_BUY_AMOUNT), valid_to)
    );
    PlacedOrder {
        hook: hook.clone(),
        signature: body["signature"].as_str().unwrap().parse().unwrap(),
        swap,
        signed,
        orderbook,
        order,
        uid,
        proxy,
        valid_to,
    }
}

/// Read what became of `placed`'s order through the wallet once the block of `settlement` is
/// final, with the orderbook stub reporting the order's trade there. The wallet must record
/// the trade in that transaction, for `payout`. Returns what its record then holds of the swap.
async fn observe_public_settlement(
    wallet: &Wallet,
    fork: &ForkChain,
    placed: &PlacedOrder,
    settlement: &TransactionReceipt,
    payout: U256,
) -> PublicSwapObservations {
    let origin = &wallet.chain;
    let owner = &wallet.destination.as_ref().unwrap().owner;
    *placed.orderbook.trade_block.lock().unwrap() = settlement.block_number;
    fork.mine(origin.finality_depth).await;
    let record = owner
        .observe_public_swap_order(
            placed.swap.operation,
            placed.swap.id,
            origin,
            &placed.orderbook.client,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let (_, saved) = record.public_swap_use(placed.swap.id).unwrap();
    let observed = saved.observations();
    let traded = observed
        .traded
        .expect("the final settlement holds the order's trade");
    assert_eq!(
        (traded.block.number, traded.transaction_hash),
        (
            settlement.block_number.unwrap(),
            Some(settlement.transaction_hash)
        )
    );
    assert_eq!(
        observed
            .trade_amounts
            .map(|amounts| (amounts.sell_amount, amounts.buy_amount)),
        Some((placed.order.sellAmount, payout))
    );
    observed
}

/// Place an order of the Public account `signer` whose hook must fail at its settlement, and
/// settle it above its buy amount. The deposit's quote is dated ahead of the fork's clock, so
/// the `SpokePool` refuses it until that time comes. The settlement must succeed and leave its
/// payout at the address of the account's proxy, which the failed hook didn't deploy, and the
/// wallet must record the proxy as holding it while the batch can still run. Returns the order
/// and the payout.
async fn held_public_order(
    wallet: &Wallet,
    fork: &ForkChain,
    destination_fork: &ForkChain,
    signer: &VaultedPublicSigner,
    route: OrderDestination,
) -> (PlacedOrder, U256) {
    let spoke_pool = wallet.chain.bridge_origin_profile().unwrap().spoke_pool();
    let placed = place_public_order(
        wallet,
        fork,
        destination_fork,
        signer,
        route,
        QUOTE_AHEAD_SECS,
    )
    .await;
    let payout = placed.order.buyAmount + U256::from(SURPLUS);
    let receipt = fork
        .settle_with_surplus(
            &placed.order,
            placed.signature.clone(),
            &[],
            std::slice::from_ref(&placed.hook),
            U256::from(SURPLUS),
        )
        .await;
    assert!(receipt.status(), "the settlement survives its failed hook");
    assert!(
        fork.timestamp().await < u64::from(placed.signed.terms.quote_timestamp),
        "the settlement's block precedes the deposit's quote"
    );
    assert!(deposits(&receipt, spoke_pool).is_empty());
    assert_eq!(fork.erc20_balance(USDC, placed.proxy).await, payout);
    assert!(fork.code(placed.proxy).await.is_empty());

    let observed = observe_public_settlement(wallet, fork, &placed, &receipt, payout).await;
    assert_eq!(
        observed.held_by_proxy,
        Some(PublicSwapProxyHolding {
            observation: observed.traded.unwrap(),
            amount: payout,
        })
    );
    assert_eq!((observed.bridge_handoff, observed.deposited), (None, None));
    assert_eq!(
        public_order_state(wallet, &placed.swap),
        Some(PublicSwapOrderState::HeldByProxy {
            amount: payout,
            batch_live: true,
        })
    );
    (placed, payout)
}

/// A Public account on Ethereum places an order that pays its cow-shed proxy, whose hook batch
/// deposits the proxy's whole balance into Across for `route`'s chain. A settlement above the
/// buy amount deposits all of its payout at the signed scale, and the fill shields the whole
/// output to the wallet on that chain.
///
/// With `route.every_outcome`, the account's next order then runs its batch on the deployed
/// proxy. Other accounts follow: one whose hook fails at the settlement and that withdraws the
/// proceeds its proxy holds, one whose order is invalidated on chain, and one whose failed
/// batch someone runs again before its deadline.
async fn public_order_scenario(route: OrderDestination) {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination_from(route.rpc_env, route.chain_id).await;
    let wallet =
        Wallet::open(&fork).with_destination_on(&destination_fork, route.chain_id, route.token);
    let origin = &wallet.chain;
    let destination = wallet.destination.as_ref().unwrap();
    let owner = &destination.owner;
    let profile = origin.public_swap_profile().unwrap();
    let (factory, settlement) = (profile.cow_shed_factory(), profile.settlement());
    let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
    let bridge = destination.chain.bridge_profile().unwrap();
    let handler = bridge.multicall_handler();
    let (buy_amount, minimum) = (
        U256::from(PUBLIC_ORDER_BUY_AMOUNT),
        U256::from(route.minimum),
    );
    let payout = buy_amount + U256::from(SURPLUS);
    // The batch scales the deposit's output from the proxy's balance, rounding down.
    let scaled = payout * minimum / buy_amount;
    assert!(scaled > minimum);
    // What the wallet's record holds of `placed`'s order after one more read of its chain.
    let observe = async |placed: &PlacedOrder| {
        owner
            .observe_public_swap_order(
                placed.swap.operation,
                placed.swap.id,
                origin,
                &placed.orderbook.client,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
    };

    deploy_swap_math(&fork).await;
    fund_relayer_with(
        &destination_fork,
        bridge.spoke_pool(),
        route.token,
        minimum * U256::from(10),
    )
    .await;

    let signer = public_signer("order");
    let source = signer.address();
    let placed = place_public_order(&wallet, &fork, &destination_fork, &signer, route, 0).await;
    let proxy = placed.proxy;

    // Anyone can run the published batch before the settlement. Its guard reverts while the
    // proxy holds less than the buy amount, and the factory deploys nothing.
    assert!(fork.code(proxy).await.is_empty());
    let early = fork
        .send_as_bypass(factory, placed.hook.call_data.clone(), BATCH_GAS)
        .await;
    assert!(!early.status(), "an early run of the batch reverts");
    assert!(fork.code(proxy).await.is_empty());

    // The solver pays more than the buy amount, and the hook deposits all of it.
    let receipt = fork
        .settle_with_surplus(
            &placed.order,
            placed.signature.clone(),
            &[],
            std::slice::from_ref(&placed.hook),
            U256::from(SURPLUS),
        )
        .await;
    assert!(
        receipt.status(),
        "the solver settles with the batch as its post-hook"
    );
    let settled_deposits = deposits(&receipt, spoke_pool);
    let [deposit] = settled_deposits.as_slice() else {
        panic!("the hook deposits once within its gas limit");
    };
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (payout, scaled)
    );
    assert_eq!(
        (deposit.depositor, deposit.recipient),
        (address_to_bytes32(source), address_to_bytes32(handler))
    );
    assert_eq!(deposit.message, placed.signed.message);
    assert_eq!(
        (
            deposit.inputToken,
            deposit.outputToken,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(USDC),
            address_to_bytes32(route.token),
            U256::from(route.chain_id)
        )
    );
    assert_eq!(fork.erc20_balance(USDC, proxy).await, U256::ZERO);
    assert!(!fork.code(proxy).await.is_empty());
    let deploying_gas = fork
        .call_gas(receipt.transaction_hash, factory, &placed.hook.call_data)
        .await
        .expect("the settlement's trace holds the hook's call");
    assert!(
        deploying_gas <= placed.hook.gas_limit,
        "the hook used {deploying_gas} gas of its limit of {}",
        placed.hook.gas_limit
    );
    println!(
        "MEASURE public_order_hook_gas chain={} proxy=deployed_by_hook used={deploying_gas} limit={}",
        route.chain_id, placed.hook.gas_limit
    );

    // The wallet reads the trade and the hand-off from the settlement once it is final, with
    // the amounts the deposit's event carries.
    let observed = observe_public_settlement(&wallet, &fork, &placed, &receipt, payout).await;
    assert_eq!(
        observed.bridge_handoff,
        Some(SwapBridgeHandoff {
            observation: observed.traded.unwrap(),
            deposit_id: Some(deposit.depositId),
        })
    );
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: payout,
            output_amount: scaled,
        })
    );
    assert_eq!(observed.held_by_proxy, None);
    assert_eq!(
        public_order_state(&wallet, &placed.swap),
        Some(PublicSwapOrderState::Bridged)
    );

    // The destination account shields the whole deposited output, not only the minimum.
    let filled = fill_and_shield(
        &destination_fork,
        destination,
        route.token,
        deposit,
        placed.swap.account,
    )
    .await;
    verify_public_delivery(&wallet, &destination_fork, &placed.swap, scaled, &filled).await;
    if !route.every_outcome {
        drop(placed);
        wallet.finish().await;
        return;
    }

    // The account's next order runs its batch on the deployed proxy, for a lower reviewed gas
    // limit. Settled at exactly its buy amount, it deposits the signed minimums.
    let next = place_public_order(&wallet, &fork, &destination_fork, &signer, route, 0).await;
    assert!(next.hook.gas_limit < placed.hook.gas_limit);
    let receipt = fork
        .settle(
            &next.order,
            next.signature.clone(),
            &[],
            std::slice::from_ref(&next.hook),
        )
        .await;
    assert!(receipt.status(), "the solver settles the next order");
    let exact = deposits(&receipt, spoke_pool);
    let [exact] = exact.as_slice() else {
        panic!("the next hook deposits once within its gas limit");
    };
    assert_eq!(
        (exact.inputAmount, exact.outputAmount),
        (buy_amount, minimum)
    );
    let deployed_gas = fork
        .call_gas(receipt.transaction_hash, factory, &next.hook.call_data)
        .await
        .expect("the settlement's trace holds the hook's call");
    assert!(
        deployed_gas <= next.hook.gas_limit,
        "the hook used {deployed_gas} gas of its limit of {}",
        next.hook.gas_limit
    );
    println!(
        "MEASURE public_order_hook_gas chain={} proxy=deployed_before used={deployed_gas} limit={}",
        route.chain_id, next.hook.gas_limit
    );
    let observed = observe_public_settlement(&wallet, &fork, &next, &receipt, buy_amount).await;
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: buy_amount,
            output_amount: minimum,
        })
    );
    assert_eq!(
        public_order_state(&wallet, &next.swap),
        Some(PublicSwapOrderState::Bridged)
    );
    drop((placed, next));

    // Another account's hook fails at the settlement, and its proxy was never deployed. The
    // account withdraws the proceeds in one transaction through the factory, which deploys
    // the proxy and runs the transfer.
    let signer = public_signer("withdrawal");
    let source = signer.address();
    let (held, payout) = held_public_order(&wallet, &fork, &destination_fork, &signer, route).await;
    let (operation, id) = (held.swap.operation, held.swap.id);
    let review = owner
        .review_public_swap_withdrawal(
            operation,
            id,
            origin,
            PUBLIC_MAX_FEE_PER_GAS,
            PUBLIC_MAX_PRIORITY_FEE_PER_GAS,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(
        (review.proxy, review.token, review.amount),
        (held.proxy, USDC, payout)
    );
    assert!(!review.proxy_deployed);
    let before = fork.erc20_balance(USDC, source).await;
    let outcome = owner
        .submit_public_swap_withdrawal_with_signer(
            operation,
            id,
            origin,
            &signer,
            PUBLIC_GAS_FEE,
            &review,
            false,
            crate::OperationHttpClient::direct_for_tests(),
            &mut |_| {},
        )
        .await
        .unwrap();
    let PublicSwapTransactionOutcome::Included {
        transaction_hash, ..
    } = outcome
    else {
        panic!("the withdrawal succeeds: {outcome:?}");
    };
    let withdrawal = fork.receipt(transaction_hash).await;
    assert_eq!(withdrawal.to, Some(factory));
    assert_eq!(
        (
            fork.erc20_balance(USDC, source).await,
            fork.erc20_balance(USDC, held.proxy).await
        ),
        (before + payout, U256::ZERO)
    );
    assert!(!fork.code(held.proxy).await.is_empty());
    assert!(
        withdrawal.gas_used <= review.gas_limit,
        "the withdrawal used {} gas of the reviewed {}",
        withdrawal.gas_used,
        review.gas_limit
    );
    println!(
        "MEASURE public_order_withdrawal_gas proxy=deployed_by_withdrawal used={} limit={}",
        withdrawal.gas_used, review.gas_limit
    );
    // The account sent its approval and this one withdrawal.
    let sent = public_swap_record(&wallet, &held.swap).0;
    assert!(matches!(
        sent.transactions(),
        [approval, withdrawal] if approval.kind == PublicSwapTransactionKind::Approval
            && withdrawal.kind == PublicSwapTransactionKind::Withdrawal
            && withdrawal.hash == transaction_hash
    ));
    // The withdrawal counts once its block is final.
    fork.mine(origin.finality_depth).await;
    observe(&held).await.unwrap();
    let withdrawn = public_swap_record(&wallet, &held.swap).0;
    assert_eq!(
        withdrawn
            .observations()
            .withdrawn
            .and_then(|observation| observation.transaction_hash),
        Some(transaction_hash)
    );
    assert_eq!(
        public_swap_order_state(&withdrawn),
        Some(PublicSwapOrderState::SwappedNotBridged)
    );
    drop(held);

    // Another account invalidates its open order on chain. Its hook batch stays signed, so a
    // second swap of the account that buys the same token is refused until the batch's
    // deadline has passed: a later payout to the proxy can't be taken by the cancelled order's
    // batch.
    let signer = public_signer("cancellation");
    let open = place_public_order(&wallet, &fork, &destination_fork, &signer, route, 0).await;
    let outcome = owner
        .submit_public_swap_invalidation_with_signer(
            open.swap.operation,
            open.swap.id,
            origin,
            &signer,
            PUBLIC_GAS_FEE,
            &open.orderbook.client,
            INVALIDATION_GAS_LIMIT,
            crate::OperationHttpClient::direct_for_tests(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, PublicSwapTransactionOutcome::Included { .. }),
        "the invalidation succeeds: {outcome:?}"
    );
    let deadline = u64::from(open.valid_to);
    // The sending step saw the invalidation's first inclusion. The settlement state in that
    // block is not final yet, so it cannot conclude cancellation or release admission.
    assert_eq!(
        public_order_state(&wallet, &open.swap),
        Some(PublicSwapOrderState::Open)
    );
    assert_eq!(
        fork.call(
            settlement,
            ForkSwapSettlement::filledAmountCall {
                orderUid: open.uid.0.to_vec().into(),
            },
        )
        .await,
        U256::MAX
    );
    fork.mine(origin.finality_depth).await;
    observe(&open).await.unwrap();
    let cancelled = public_swap_record(&wallet, &open.swap).0;
    assert_eq!(
        public_swap_order_state(&cancelled),
        Some(PublicSwapOrderState::Cancelled {
            retry_at: deadline + 1
        })
    );
    assert_eq!(
        cancelled.observations().cancelled.unwrap().transaction_hash,
        None
    );
    let refused = owner
        .claim_public_swap(public_claim(
            SwapUseId::random().unwrap(),
            ExecutorOperationId::random().unwrap(),
            signer.address(),
            open.swap.terms,
            open.swap.plan.max_gas_cost,
        ))
        .unwrap_err();
    assert!(
        matches!(
            refused.downcast_ref::<ExecutorStoreError>(),
            Some(ExecutorStoreError::PublicSwapBuysSameToken {
                swap,
                chain_id,
                available_at: Some(available_at),
            }) if *swap == open.swap.id
                && *chain_id == route.chain_id
                && *available_at == deadline + 1
        ),
        "the second swap is refused until the batch's deadline: {refused}"
    );
    drop(open);

    // Another account's hook fails at the settlement because its deposit's quote is dated
    // ahead of the fork's clock. Once that time has come, someone runs the same batch again
    // before its deadline, and it deposits what the proxy held.
    let signer = public_signer("late batch");
    let source = signer.address();
    let (late, payout) = held_public_order(&wallet, &fork, &destination_fork, &signer, route).await;
    mine_past(&fork, u64::from(late.signed.terms.quote_timestamp)).await;
    let run = fork
        .send_as_bypass(factory, late.hook.call_data.clone(), BATCH_GAS)
        .await;
    assert!(
        run.status(),
        "the same batch deposits once the pool accepts its quote"
    );
    let deadline = u64::from(late.valid_to);
    assert!(
        fork.timestamp().await <= deadline,
        "the batch ran before its deadline"
    );
    let deposits = deposits(&run, spoke_pool);
    let [deposit] = deposits.as_slice() else {
        panic!("the late run deposits once");
    };
    assert_eq!(
        (deposit.depositor, deposit.inputAmount, deposit.outputAmount),
        (address_to_bytes32(source), payout, scaled)
    );
    assert_eq!(fork.erc20_balance(USDC, late.proxy).await, U256::ZERO);
    assert!(!fork.code(late.proxy).await.is_empty());

    // The batch can run until its deadline, so the wallet looks for no deposit before the
    // first block past it is final. Then it finds this one among the Public account's
    // deposits, and the swap is bridged.
    fork.mine(origin.finality_depth).await;
    observe(&late).await.unwrap();
    assert_eq!(
        public_order_state(&wallet, &late.swap),
        Some(PublicSwapOrderState::HeldByProxy {
            amount: payout,
            batch_live: true,
        })
    );
    mine_past(&fork, deadline).await;
    fork.mine(origin.finality_depth).await;
    observe(&late).await.unwrap();
    let observed = public_swap_record(&wallet, &late.swap).0.observations();
    assert_eq!(
        observed.bridge_handoff,
        Some(SwapBridgeHandoff {
            observation: SwapObservation {
                block: BlockNumHash::new(run.block_number.unwrap(), run.block_hash.unwrap()),
                transaction_hash: Some(run.transaction_hash),
            },
            deposit_id: Some(deposit.depositId),
        })
    );
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: payout,
            output_amount: scaled,
        })
    );
    assert_eq!(
        public_order_state(&wallet, &late.swap),
        Some(PublicSwapOrderState::Bridged)
    );
    let filled = fill_and_shield(
        &destination_fork,
        destination,
        route.token,
        deposit,
        late.swap.account,
    )
    .await;
    verify_public_delivery(&wallet, &destination_fork, &late.swap, scaled, &filled).await;
    drop(late);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_order_deposits_its_whole_payout_and_is_shielded() {
    Box::pin(public_order_scenario(OrderDestination {
        chain_id: DESTINATION_CHAIN,
        rpc_env: DESTINATION_FORK_RPC_URL_ENV,
        token: DESTINATION_TOKEN,
        minimum: 14_900_000,
        every_outcome: true,
    }))
    .await;
}

// BNB Chain's USDC has 18 decimals, so the approved minimum and the output the batch scales
// from the payout are in units a trillion times smaller than the 6-decimal USDC the order buys.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, BNB_FORK_RPC_URL and anvil"]
async fn swap_fork_public_order_scales_its_output_to_an_18_decimal_token() {
    Box::pin(public_order_scenario(OrderDestination {
        chain_id: BNB_CHAIN,
        rpc_env: BNB_FORK_RPC_URL_ENV,
        token: BNB_USDC,
        minimum: 14_900_000_000_000_000_000,
        every_outcome: false,
    }))
    .await;
}

/// A Public account on `route`'s chain that holds USDC and no native balance sells it for the
/// chain's wrapped native token, delivered to Polygon. The review plans a permit and no gas
/// from the account, the approvals step sends nothing, and the order carries the signed permit
/// as its pre-hook. The solver's settlement runs that hook through the trampoline, takes the
/// USDC, and runs the post-hook's deposit, which the wallet reads as the swap's hand-off. The
/// account sends no transaction at any point.
async fn public_permit_order_scenario(route: PermitOrigin) {
    use broadcaster_core::contracts::erc20_permit::IERC20Permit;

    let fork = ForkChain::start_origin(route.rpc_env, route.chain_id).await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let wallet = Wallet::open_on(&fork, route.chain_id).with_destination(&destination_fork);
    let origin = &wallet.chain;
    let destination = wallet.destination.as_ref().unwrap();
    let owner = &destination.owner;
    let profile = origin.public_swap_profile().unwrap();
    let (factory, vault_relayer) = (profile.cow_shed_factory(), profile.vault_relayer());
    let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
    let bridge = destination.chain.bridge_profile().unwrap();
    let usdc = route.usdc;
    let weth = crate::amounts::wrapped_native_token_for_chain(route.chain_id).unwrap();
    let sell_amount = U256::from(PERMIT_SELL_AMOUNT);
    // The harness settles at the mainnet addresses, which this chain must share.
    assert_eq!(profile.settlement(), SETTLEMENT);
    assert!(
        !fork.code(HOOKS_TRAMPOLINE).await.is_empty(),
        "chain {} has the hooks trampoline",
        route.chain_id
    );
    // anvil runs no Nitro precompile. This one answers every call with an L1 base fee of
    // 0.1 gwei, which only enters the order's gas estimate.
    if route.chain_id == ARBITRUM_ONE {
        fork.set_code(
            ARB_GAS_INFO,
            alloy::primitives::bytes!("6305f5e1005f5260205ff3"),
        )
        .await;
    }
    deploy_swap_math(&fork).await;

    // The account holds the USDC it sells and nothing else: no native balance, no allowance
    // to the vault relayer, and no transaction sent.
    let signer = public_signer("permit");
    let source = signer.address();
    let proxy = proxy_address(factory, profile.cow_shed_implementation(), source);
    fork.add_erc20(usdc, source, sell_amount).await;
    let allowance = || ForkAllowance::allowanceCall {
        owner: source,
        spender: vault_relayer,
    };
    let permit_nonce = || IERC20Permit::noncesCall { owner: source };
    assert_eq!(fork.erc20_balance(usdc, source).await, sell_amount);
    assert_eq!(fork.native_balance(source).await, U256::ZERO);
    assert_eq!(fork.call(usdc, allowance()).await, U256::ZERO);
    assert_eq!(fork.transaction_count(source).await, 0);
    let nonce = fork.call(usdc, permit_nonce()).await;

    // The review reads the token's permit on the fork. `CoW`'s quote and Across's preview
    // come from local stubs.
    let (quote_url, _, quote_stub) = spawn_bridge_stub(move |_| {
        json!({
            "quote": {
                "sellToken": usdc, "buyToken": weth,
                "sellAmount": sell_amount.to_string(),
                "buyAmount": PERMIT_QUOTED_BUY.to_string(), "validTo": 1,
                "feeAmount": "0", "gasAmount": "150000", "gasPrice": "1000000000",
                "sellTokenPrice": "300000000", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        })
        .to_string()
    })
    .await;
    let quote_client = CowOrderbookClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        quote_url,
        route.chain_id,
    )
    .unwrap();
    let (destination_spoke_pool, previewed) = (bridge.spoke_pool(), fork.timestamp().await);
    let (preview_url, _, preview_stub) = spawn_bridge_stub(move |_| {
        across_quote_at(
            spoke_pool,
            destination_spoke_pool,
            U256::from(PERMIT_PREVIEW_OUTPUT),
            previewed,
        )
    })
    .await;
    let registry = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let target = crate::bridge::PublicBridgeDestination {
        destination: crate::bridge::BridgeDestination {
            destination_token: POLYGON_WETH,
            intermediate: weth,
            symbol: "WETH".into(),
            same_asset: true,
            near: None,
        },
        path: crate::bridge::PublicBridgePath::Order,
    };
    let review = owner
        .review_public_swap(crate::PublicSwapReviewRequest {
            origin,
            source,
            sell: crate::bridge::PublicSellAsset::Erc20(usdc),
            sell_amount,
            destination: &target,
            slippage_bps: 50,
            gas_share_bps: 5_000,
            on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
            orderbook: Some(&quote_client),
            across: &across_client(preview_url),
            anchor_cache: None,
            token_registry: &registry,
            anchor_deviation_bps: 300,
            max_fee_per_gas: PUBLIC_MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: PUBLIC_MAX_PRIORITY_FEE_PER_GAS,
            client: crate::OperationHttpClient::direct_for_tests(),
        })
        .await
        .unwrap();
    quote_stub.abort();
    preview_stub.abort();
    assert!(
        review.signs_permit(),
        "the review plans a permit for USDC's short allowance"
    );
    let plan = review.gas_plan().clone();
    assert_eq!(plan.approval_gas_limits, [] as [u64; 0]);
    assert_eq!(
        (plan.deposit_gas_limit, plan.max_gas_cost),
        (None, U256::ZERO)
    );

    // The swap is claimed with the review's own approval, which prices the permit pre-hook.
    // No anchor checked the stub's price, so the approval acknowledges it.
    let approval = review
        .approval(
            SwapApprovedAccount {
                address: None,
                setup: true,
            },
            Some(setup_fee_limit(1_000)),
            true,
        )
        .unwrap();
    let permit_gas_limit = approval.bounds.pre_hook_gas_limit;
    assert_eq!(
        permit_gas_limit,
        crate::cow::hook_gas_limit(crate::cow::PUBLIC_PERMIT_HOOK_GAS)
    );
    let terms = PublicTerms {
        sell_token: usdc,
        sell_amount,
        bridged_token: weth,
        bridged_amount: review.buy_amount().unwrap(),
        destination_token: POLYGON_WETH,
        destination_minimum: review.bridge().destination_minimum,
        hook_gas_limit: review.hook_gas_limit(),
    };
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let claim = PublicSwapUseClaim {
        id,
        origin_chain: route.chain_id,
        source,
        source_scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: TEST_WALLET_ID.to_owned(),
        },
        account: SwapAccountChoice::New(operation),
        destination_token: POLYGON_WETH,
        intent: review.intent(),
        approval,
    };
    let swap = wallet
        .claimed_public_swap(&destination_fork, operation, claim, terms, plan)
        .await;

    // The approvals step has nothing to send.
    let outcome = owner
        .submit_public_swap_approvals_with_signer(
            swap.operation,
            swap.id,
            origin,
            &signer,
            PUBLIC_GAS_FEE,
            crate::OperationHttpClient::direct_for_tests(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome, PublicSwapApprovalsOutcome::Ready);
    assert_eq!(
        public_swap_record(&wallet, &swap).0.transactions(),
        [] as [PublicSwapTransaction; 0]
    );

    // The order signs the permit under the account's current nonce until the order's
    // `validTo`, proves it on the token, and carries it as its one pre-hook.
    fork.mine(1).await;
    let quoted = fork.timestamp().await;
    let valid_to = valid_until(PUBLIC_VALID_SECS);
    let signed = wallet.sign_public_delivery(&swap, quoted, valid_to).await;
    let orderbook = spawn_public_orderbook(route.chain_id, profile.settlement()).await;
    let outcome = owner
        .submit_public_swap_order_with_signer(
            swap.operation,
            swap.id,
            origin,
            &signer,
            &orderbook.client,
            &signed.delivery,
            &signed.terms,
            valid_to,
            new_public_swap_batch_nonce().unwrap(),
            review.quote_id(),
            false,
        )
        .await
        .unwrap();
    let PublicSwapOrderOutcome::Submitted { uid } = outcome else {
        panic!("the order is submitted: {outcome:?}");
    };
    let body = orderbook.orders.lock().unwrap().last().cloned().unwrap();
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    let ([pre], [post]) = (hooks.pre.as_slice(), hooks.post.as_slice()) else {
        panic!("the order has one pre-hook and one post-hook");
    };
    assert_eq!((pre.target, pre.gas_limit), (usdc, permit_gas_limit));
    let permit = IERC20Permit::permitCall::abi_decode(&pre.call_data).unwrap();
    assert_eq!(
        (permit.owner, permit.spender, permit.value, permit.deadline),
        (source, vault_relayer, sell_amount, U256::from(valid_to))
    );
    assert_eq!(
        (post.target, Some(post.gas_limit)),
        (factory, terms.hook_gas_limit)
    );
    let order = submitted_order(&body);
    assert_eq!(
        (order.sellToken, order.buyToken, order.receiver),
        (usdc, weth, proxy)
    );
    assert_eq!(
        (order.sellAmount, order.buyAmount, order.validTo),
        (sell_amount, terms.bridged_amount, valid_to)
    );
    let recorded = public_swap_record(&wallet, &swap).0;
    assert_eq!(
        recorded
            .order()
            .and_then(|order| order.permit())
            .map(|permit| (permit.nonce(), permit.value(), permit.deadline())),
        Some((nonce, sell_amount, valid_to))
    );
    // Signing and proving the permit changed nothing on chain.
    assert_eq!(fork.call(usdc, permit_nonce()).await, nonce);
    assert_eq!(fork.call(usdc, allowance()).await, U256::ZERO);
    assert_eq!(fork.transaction_count(source).await, 0);
    let placed = PlacedOrder {
        hook: post.clone(),
        signature: body["signature"].as_str().unwrap().parse().unwrap(),
        swap,
        signed,
        orderbook,
        order,
        uid,
        proxy,
        valid_to,
    };

    // The solver settles with both hooks. The settlement takes the USDC only if the permit
    // ran first: nothing else gave the vault relayer an allowance.
    let receipt = fork
        .settle(
            &placed.order,
            placed.signature.clone(),
            &hooks.pre,
            std::slice::from_ref(&placed.hook),
        )
        .await;
    assert!(
        receipt.status(),
        "the solver settles with the permit as its pre-hook and the batch as its post-hook"
    );
    assert_eq!(fork.call(usdc, permit_nonce()).await, nonce + U256::ONE);
    assert_eq!(fork.erc20_balance(usdc, source).await, U256::ZERO);
    assert_eq!(fork.call(usdc, allowance()).await, U256::ZERO);
    let settled = deposits(&receipt, spoke_pool);
    let [deposit] = settled.as_slice() else {
        panic!("the post-hook deposits once within its gas limit");
    };
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (terms.bridged_amount, terms.destination_minimum)
    );
    assert_eq!(
        (deposit.depositor, deposit.recipient),
        (
            address_to_bytes32(source),
            address_to_bytes32(bridge.multicall_handler())
        )
    );
    assert_eq!(deposit.message, placed.signed.message);
    assert_eq!(
        (
            deposit.inputToken,
            deposit.outputToken,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(weth),
            address_to_bytes32(POLYGON_WETH),
            U256::from(DESTINATION_CHAIN)
        )
    );
    // The trampoline's call of the token, from the settlement's trace.
    match fork
        .call_gas(receipt.transaction_hash, usdc, &pre.call_data)
        .await
    {
        Some(used) => {
            assert!(
                used <= pre.gas_limit,
                "the permit used {used} gas of its limit of {}",
                pre.gas_limit
            );
            eprintln!(
                "MEASURE public_permit_hook_gas chain={} used={used} limit={}",
                route.chain_id, pre.gas_limit
            );
        }
        None => eprintln!(
            "MEASURE public_permit_hook_gas chain={} unavailable: no permit call was traced",
            route.chain_id
        ),
    }

    // The wallet reads the trade and the hand-off from the final settlement.
    let observed =
        observe_public_settlement(&wallet, &fork, &placed, &receipt, terms.bridged_amount).await;
    assert_eq!(
        observed.bridge_handoff,
        Some(SwapBridgeHandoff {
            observation: observed.traded.unwrap(),
            deposit_id: Some(deposit.depositId),
        })
    );
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: terms.bridged_amount,
            output_amount: terms.destination_minimum,
        })
    );
    assert_eq!(observed.held_by_proxy, None);
    assert_eq!(
        public_order_state(&wallet, &placed.swap),
        Some(PublicSwapOrderState::Bridged)
    );
    // From the review to the hand-off the account sent nothing and held no native balance.
    assert_eq!(fork.transaction_count(source).await, 0);
    assert_eq!(fork.native_balance(source).await, U256::ZERO);
    drop(placed);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_permit_order_on_ethereum_sends_nothing_from_the_account() {
    Box::pin(public_permit_order_scenario(PermitOrigin {
        chain_id: 1,
        rpc_env: FORK_RPC_URL_ENV,
        usdc: USDC,
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs BASE_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_permit_order_on_base_sends_nothing_from_the_account() {
    Box::pin(public_permit_order_scenario(PermitOrigin {
        chain_id: BASE,
        rpc_env: BASE_FORK_RPC_URL_ENV,
        usdc: BASE_USDC,
    }))
    .await;
}

#[tokio::test]
#[ignore = "needs ARBITRUM_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_permit_order_on_arbitrum_sends_nothing_from_the_account() {
    Box::pin(public_permit_order_scenario(PermitOrigin {
        chain_id: ARBITRUM_ONE,
        rpc_env: ARBITRUM_FORK_RPC_URL_ENV,
        usdc: ARBITRUM_USDC,
    }))
    .await;
}
