use super::*;

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_settlement_with_both_hooks_completes_at_finality() {
    Box::pin(settlement_with_both_hooks_scenario(
        1,
        FORK_RPC_URL_ENV,
        USDC,
    ))
    .await;
}

#[tokio::test]
#[ignore = "needs BASE_FORK_RPC_URL and anvil"]
async fn swap_fork_settlement_on_base_with_both_hooks_completes_at_finality() {
    Box::pin(settlement_with_both_hooks_scenario(
        BASE,
        BASE_FORK_RPC_URL_ENV,
        BASE_USDC,
    ))
    .await;
}

/// A Reshield order selling the chain `chain_id`'s WETH for its `usdc`, on a fork from the RPC
/// that `rpc_env` names, settles with both hooks: the pre-hook runs the deadline guard, the
/// trade completes and the post-hook shields the bought USDC to the chain's Railgun, which the
/// wallet observes as Done at finality.
async fn settlement_with_both_hooks_scenario(chain_id: u64, rpc_env: &str, usdc: Address) {
    let fork = ForkChain::start_origin(rpc_env, chain_id).await;
    let mut wallet = Wallet::open_on(&fork, chain_id);
    wallet.pair.0 = usdc;
    let railgun = wallet.chain.require_railgun().unwrap().deployment.contract;
    // The pre-hook unshields from Railgun's own balance, which a newer deployment may lack.
    let (weth, sold) = (wallet.weth(), U256::from(SELL_AMOUNT));
    if fork.erc20_balance(weth, railgun).await < sold {
        fork.add_erc20(weth, railgun, sold).await;
    }
    let swap = wallet.swap(&fork).await;
    // Balanced share of the stub quote: the best case is 20,000,000 plus the 20,020 network
    // fee, less 0.5% tolerance (19,919,919), less 25% of a one-unit gas estimate at 1 wei.
    // The buy amount is the smallest one that nets that minimum after the shield fee.
    assert_eq!(swap.order.buyAmount, U256::from(19_919_918));

    let receipt = fork
        .settle(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
        )
        .await;
    assert!(receipt.status(), "the solver settles with both hooks");
    assert_eq!(fork.erc20_balance(usdc, swap.executor).await, U256::ZERO);
    let settled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;

    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    let observed = first_order(&record).observations();
    let settlement = Some(receipt.transaction_hash);
    assert_eq!(
        observed
            .pre_hook_executed
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        observed
            .traded
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        observed
            .delivered
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::Done
    );
    assert!(record.reserved_inputs().contains(&swap.input));

    // A blocked reshield is refunded to the stealth account that funded it, not the solver.
    let shielded = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == railgun)
        .filter_map(|log| log.log_decode::<Shield>().ok())
        .flat_map(|log| log.inner.data.commitments)
        .find(|preimage| preimage.token.tokenAddress == usdc)
        .map(|preimage| U256::from(preimage.value))
        .expect("the post-hook shields the bought USDC");
    let http = HttpContext::direct_for_tests();
    let origin = crate::resolve_source_tx_origin(
        chain_id,
        &wallet.chain,
        settled,
        receipt.transaction_hash,
        usdc,
        shielded,
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(origin, swap.executor);
    assert_ne!(origin, receipt.from);
    wallet.finish().await;
}

/// `holder`'s balance of `token`, or of ETH for the native marker `Address::ZERO`.
async fn balance_of(fork: &ForkChain, token: Address, holder: Address) -> U256 {
    if token == Address::ZERO {
        fork.native_balance(holder).await
    } else {
        fork.erc20_balance(token, holder).await
    }
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_external_settlement_pays_the_receiver_and_completes_on_the_trade() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let receiver = Address::repeat_byte(0x77);
    // An ERC-20 buy, then a native buy that the settlement pays from its ETH balance.
    for buy in [USDC, Address::ZERO] {
        wallet.pair = (buy, SwapDelivery::External { receiver });
        let swap = wallet.swap(&fork).await;
        assert_eq!(swap.order.receiver, receiver);
        assert_eq!(swap.hooks.post, [] as [AppDataHook; 0]);
        let before = balance_of(&fork, buy, receiver).await;

        let receipt = fork
            .settle(&swap.order, swap.signature.clone(), &swap.hooks.pre, &[])
            .await;
        assert!(
            receipt.status(),
            "the solver settles with the pre-hook alone"
        );
        assert!(balance_of(&fork, buy, receiver).await >= before + swap.order.buyAmount);
        assert_eq!(balance_of(&fork, buy, swap.executor).await, U256::ZERO);
        let settled = receipt.block_number.unwrap();
        fork.mine(wallet.chain.finality_depth).await;

        // The deployed settlement's Trade event alone completes the order.
        wallet
            .owner
            .observe_swap_settlement(
                swap.operation,
                swap.uid,
                settled,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        let record = wallet
            .owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == swap.operation)
            .unwrap();
        assert_eq!(
            swap_order_state(record.swap().unwrap(), first_order(&record)),
            SwapOrderState::Done
        );
        assert_eq!(
            first_order(&record)
                .observations()
                .delivered
                .and_then(|observation| observation.transaction_hash),
            Some(receipt.transaction_hash)
        );
    }
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_early_pre_hook_then_fill_completes() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let swap = wallet.swap(&fork).await;

    // Someone runs the published pre-hook before a solver settles the order.
    let pre_hook = &swap.hooks.pre[0];
    let early = fork
        .send_as_bypass(
            swap.executor,
            pre_hook.call_data.clone(),
            pre_hook.gas_limit + 200_000,
        )
        .await;
    assert!(early.status(), "the pre-hook runs before its deadline");
    let ran = early.block_number.unwrap();
    // CoW's autopilot drops an order whose owner's balance or allowance after the pre-hook is
    // below `sellAmount`. The order sells exactly what the unshield left after Railgun's fee.
    let profile = wallet.chain.swap_profile().unwrap();
    assert_eq!(
        fork.erc20_balance(WETH, swap.executor).await,
        swap.order.sellAmount
    );
    assert_eq!(
        fork.call(
            WETH,
            ForkAllowance::allowanceCall {
                owner: swap.executor,
                spender: profile.vault_relayer(),
            },
        )
        .await,
        swap.order.sellAmount
    );
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, ran..ran + 1).await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::PreHookOnly { expired: false }
    );

    // The funds and approval are already in place, so the solver settles without it.
    let receipt = fork
        .settle(&swap.order, swap.signature.clone(), &[], &swap.hooks.post)
        .await;
    assert!(receipt.status(), "the solver fills the order");
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    let settled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;

    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    let observed = first_order(&record).observations();
    assert_eq!(
        observed
            .pre_hook_executed
            .and_then(|observation| observation.transaction_hash),
        Some(early.transaction_hash)
    );
    let settlement = Some(receipt.transaction_hash);
    assert_eq!(
        observed
            .traded
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        observed
            .delivered
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::Done
    );
    assert!(record.reserved_inputs().contains(&swap.input));
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_early_pre_hook_then_speed_up_fill_completes() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let (first, second) = Box::pin(wallet.overlap(&fork)).await;
    let (operation, executor) = (first.operation, first.executor);
    let sell_amount = first.order.sellAmount;

    // Someone runs the open order's pre-hook alone, before any settlement.
    let early = run_pre_hook(&fork, &first).await;
    assert!(
        early.status(),
        "the earlier pre-hook runs before its deadline"
    );
    let ran = early.block_number.unwrap();
    // The speed-up's pre-hook shares that nonce and proof, so it can no longer execute, and
    // the open order it would invalidate stays fillable.
    assert!(
        !run_pre_hook(&fork, &second).await.status(),
        "the shared nonce and proof fund the account once"
    );
    assert_eq!(fork.erc20_balance(WETH, executor).await, sell_amount);
    assert_eq!(
        relayer_allowance(&fork, &wallet, executor).await,
        sell_amount
    );
    assert_eq!(order_filled(&fork, &wallet, first.uid).await, U256::ZERO);

    // Without a trade the proof's effects show the funding and no hook. Both orders read as
    // funded and can still fill.
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(operation, ran..ran + 1).await;
    assert_shared_funding(&record, [&first, &second], early.transaction_hash);
    for swap in [&first, &second] {
        let order = order_of(&record, swap.uid);
        assert!(order.observations().traded.is_none());
        assert_eq!(
            swap_order_state(record.swap().unwrap(), order),
            SwapOrderState::PreHookOnly { expired: false }
        );
    }

    // A solver settles the speed-up with its app-data hooks. Its pre-hook reverts inside the
    // trampoline and the trade takes the funding the earlier pre-hook left.
    let receipt = fork
        .settle(
            &second.order,
            second.signature.clone(),
            &second.hooks.pre,
            &second.hooks.post,
        )
        .await;
    assert!(receipt.status(), "the solver fills the speed-up");
    assert_eq!(order_filled(&fork, &wallet, second.uid).await, sell_amount);
    assert_eq!(fork.erc20_balance(USDC, executor).await, U256::ZERO);
    // The trade used the whole exact allowance. The open order was never invalidated, and
    // still can't fill: nothing is left for the vault relayer to take.
    assert_eq!(fork.erc20_balance(WETH, executor).await, U256::ZERO);
    assert_eq!(
        relayer_allowance(&fork, &wallet, executor).await,
        U256::ZERO
    );
    assert_eq!(order_filled(&fork, &wallet, first.uid).await, U256::ZERO);
    let second_fill = fork
        .settle(
            &first.order,
            first.signature.clone(),
            &first.hooks.pre,
            &first.hooks.post,
        )
        .await;
    assert!(
        !second_fill.status(),
        "at most one of the two orders trades"
    );
    let settled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;

    // The trade and the delivery are the speed-up's. Its pre-hook is not recorded as executed,
    // and neither is the open order's: the funding still names no hook.
    let record = wallet.observe(operation, settled..settled + 1).await;
    let settlement = Some(receipt.transaction_hash);
    let observed = order_of(&record, second.uid).observations();
    assert_eq!(
        observed
            .traded
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        observed
            .delivered
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        swap_order_state(record.swap().unwrap(), order_of(&record, second.uid)),
        SwapOrderState::Done
    );
    assert!(order_of(&record, first.uid).observations().traded.is_none());
    // The open order can't fill any more, and the delivered trade completes the use.
    assert_eq!(
        swap_order_state(record.swap().unwrap(), order_of(&record, first.uid)),
        SwapOrderState::Superseded
    );
    assert!(record.swap().unwrap().admits_attempt());
    assert_shared_funding(&record, [&first, &second], early.transaction_hash);
    assert!(record.reserved_inputs().contains(&first.input));
    wallet.finish().await;
}

/// One settlement fills one of two overlapping Reshield orders with that order's pre-hook: the
/// speed-up when `speed_up_trades`, else the open order. Its post-hook slot runs the speed-up's
/// post-hook when `speed_up_shields`, else the open order's. Either one shields the account's
/// whole balance of the bought token to the wallet, at the nonce after the shared one.
async fn speed_up_fill_scenario(speed_up_trades: bool, speed_up_shields: bool) {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let (first, second) = Box::pin(wallet.overlap(&fork)).await;
    let (operation, executor) = (first.operation, first.executor);
    let shared_nonce = pre_hook_nonce(&wallet.record(operation));
    let (traded, unfilled) = if speed_up_trades {
        (&second, &first)
    } else {
        (&first, &second)
    };
    let post_hooks = if speed_up_shields {
        &second.hooks.post
    } else {
        &first.hooks.post
    };

    let receipt = fork
        .settle(
            &traded.order,
            traded.signature.clone(),
            &traded.hooks.pre,
            post_hooks,
        )
        .await;
    assert!(receipt.status(), "the solver settles one of the two orders");
    // The pre-hook funded the sell amount once, and the trade took all of it under the exact
    // allowance.
    assert_eq!(fork.erc20_balance(WETH, executor).await, U256::ZERO);
    assert_eq!(
        relayer_allowance(&fork, &wallet, executor).await,
        U256::ZERO
    );
    assert_eq!(
        order_filled(&fork, &wallet, traded.uid).await,
        traded.order.sellAmount
    );
    // Only the speed-up's pre-hook invalidates. Where it ran, the open order reads as
    // invalidated. Where the open order's ran, the speed-up reads as unfilled.
    assert_eq!(
        order_filled(&fork, &wallet, unfilled.uid).await,
        if speed_up_trades {
            U256::MAX
        } else {
            U256::ZERO
        }
    );
    // The post-hook shielded the whole payout to the wallet and took the next nonce.
    let shields = shields_of(&receipt, RAILGUN, USDC);
    let [(credited, fee)] = shields.as_slice() else {
        panic!("the post-hook shields the payout once");
    };
    let (credited, fee) = (*credited, *fee);
    assert_eq!(credited + fee, traded.order.buyAmount);
    assert_eq!(fork.erc20_balance(USDC, executor).await, U256::ZERO);
    assert_eq!(
        execution_nonce(&fork, executor).await,
        shared_nonce + U256::from(2)
    );

    // The other order can neither fund the account again nor fill.
    assert!(
        !run_pre_hook(&fork, unfilled).await.status(),
        "the shared nonce is consumed"
    );
    let second_fill = fork
        .settle(
            &unfilled.order,
            unfilled.signature.clone(),
            &unfilled.hooks.pre,
            &unfilled.hooks.post,
        )
        .await;
    assert!(
        !second_fill.status(),
        "at most one of the two orders trades"
    );
    assert_eq!(fork.erc20_balance(WETH, executor).await, U256::ZERO);

    // Routine confirmation reads the settlement's receipts for the order that traded. The
    // credit meets that order's approved minimum, whichever post-hook shielded it.
    let settled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    wallet
        .owner
        .observe_swap_settlement(
            operation,
            traded.uid,
            settled,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let record = wallet.record(operation);
    let settlement = Some(receipt.transaction_hash);
    let confirmed = order_of(&record, traded.uid).observations();
    assert_eq!(
        confirmed
            .traded
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert!(
        order_of(&record, unfilled.uid)
            .observations()
            .traded
            .is_none()
    );
    assert_eq!(
        swap_order_state(record.swap().unwrap(), order_of(&record, unfilled.uid)),
        SwapOrderState::Superseded
    );
    assert!(credited >= order_of(&record, traded.uid).bounds().private_minimum);
    if speed_up_trades == speed_up_shields {
        // The traded order's own post-hook: the receipt alone establishes the delivery.
        assert_eq!(
            confirmed
                .settlement_credit
                .map(|credit| (credit.private_amount, credit.fee)),
            Some((credited, Some(fee)))
        );
        assert_eq!(confirmed.delivered, confirmed.traded);
    }

    // Reconciliation completes the swap on the verified trade and the shield that followed
    // it. The other order holds neither, and the shared proof's effects name no hook.
    let record = wallet.observe(operation, settled..settled + 1).await;
    let (won, lost) = (
        order_of(&record, traded.uid),
        order_of(&record, unfilled.uid),
    );
    assert_eq!(
        won.observations()
            .traded
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        won.observations()
            .delivered
            .and_then(|observation| observation.transaction_hash),
        settlement
    );
    assert_eq!(
        swap_order_state(record.swap().unwrap(), won),
        SwapOrderState::Done
    );
    assert!(lost.observations().traded.is_none() && lost.observations().delivered.is_none());
    assert_eq!(
        swap_order_state(record.swap().unwrap(), lost),
        SwapOrderState::Superseded
    );
    assert!(record.swap().unwrap().admits_attempt());
    assert_shared_funding(&record, [&first, &second], receipt.transaction_hash);
    assert!(record.reserved_inputs().contains(&first.input));
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_speed_up_fill_invalidates_the_open_order() {
    Box::pin(speed_up_fill_scenario(true, true)).await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_open_order_fill_leaves_its_speed_up_unfilled() {
    Box::pin(speed_up_fill_scenario(false, false)).await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_open_order_fill_is_shielded_by_its_speed_ups_post_hook() {
    Box::pin(speed_up_fill_scenario(false, true)).await;
}
