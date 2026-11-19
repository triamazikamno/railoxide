use super::*;

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_early_funded_post_hook_leaves_proceeds_for_recovery() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let swap = wallet.swap(&fork).await;

    // Someone funds the executor, and the solver runs the post-hook after the trade is
    // recorded but before the payout. The post-hook's guard passes on that funding and
    // shields it, then the settlement pays the proceeds to the executor. The trade and
    // the post-hook's shield land in one block, so only the balance shows the proceeds
    // are still public.
    fork.add_erc20(USDC, swap.executor, swap.order.buyAmount)
        .await;
    let receipt = fork
        .settle_with_intra_hooks(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
            &[],
        )
        .await;
    assert!(
        receipt.status(),
        "the solver runs the post-hook before the payout"
    );
    let settled = receipt.block_number.unwrap();
    // A reverted post-hook would have left the funding next to the proceeds.
    let held = fork.erc20_balance(USDC, swap.executor).await;
    assert_eq!(held, swap.order.buyAmount);
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    assert_eq!(
        first_order(&record)
            .observations()
            .traded
            .and_then(|observation| observation.transaction_hash),
        Some(receipt.transaction_hash)
    );
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::NotDelivered
    );

    // Recovery is offered. Both hooks used their nonces and the traded order can't fill
    // again, so the batch only shields, at k + 2.
    let prepared = wallet
        .prepare_recovery(swap.operation, Some(USDC), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::from(2)
        }
    );
    assert_eq!(prepared.calls().len(), 1);
    assert!(shields(&prepared.calls()[0], swap.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    assert!(
        deliver(&fork, &issued).await.status(),
        "the recovery shields the proceeds"
    );
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_retry_invalidates_an_order_stalled_by_an_older_post_hook() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let first = wallet.swap(&fork).await;
    let (operation, executor) = (first.operation, first.executor);
    let profile = wallet.chain.swap_profile().unwrap();
    let store = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1).unwrap();
    let state_of = |record: &ExecutorRecord, index: usize| {
        let swap = record.swap().unwrap();
        swap_order_state(swap, &swap.orders()[index])
    };
    let filled = |uid: OrderUid| {
        fork.call(
            profile.settlement(),
            ForkSwapSettlement::filledAmountCall {
                orderUid: uid.0.to_vec().into(),
            },
        )
    };

    // A cancellation wins the first pre-hook's nonce k and invalidates its order.
    let current = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let observed = current.nonce_observation().unwrap();
    let tree = fork.railgun_tree().await;
    let fee = wallet.note(tree, U256::ONE);
    let cancellation = railgun_wallet::TransactionCall {
        to: executor,
        data: RelayAdapt7702::executeCall {
            _transactions: vec![synthetic_transaction(
                tree,
                wallet.nullifier(&fee),
                executor,
                None,
            )],
            _actionData: RelayAdapt7702ActionData {
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls: vec![Call {
                    to: profile.settlement(),
                    value: U256::ZERO,
                    data: GPv2Settlement::invalidateOrderCall {
                        orderUid: first.uid.0.to_vec().into(),
                    }
                    .abi_encode()
                    .into(),
                }],
            },
            _nonce: observed.nonce(),
            _signature: Bytes::new(),
        }
        .abi_encode()
        .into(),
    };
    let (hash, signed) = wallet
        .owner
        .sign_swap_execute_for_tests(operation, &password(), &cancellation)
        .unwrap();
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                observed.nonce(),
                current.delegate(),
                hash,
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(
                    signed.clone(),
                    observed,
                    vec![ExecutorInputIdentity::from_utxo(&fee)],
                ),
            ),
        )
        .unwrap();
    let receipt = fork.send_as_bypass(executor, signed, 3_000_000).await;
    assert!(receipt.status(), "the cancellation wins nonce k");
    let cancelled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    // Private sync shows the cancellation's fee note spent in its recorded transaction, which
    // is what names it at nonce k.
    store
        .record_submission(operation, hash, receipt.transaction_hash)
        .unwrap();
    let mut spent = railgun_wallet::WalletUtxo::new(fee);
    spent.spent = Some(UtxoSource {
        tx_hash: receipt.transaction_hash,
        block_number: cancelled,
        block_timestamp: 0,
    });
    let sync = sync_service::WalletCurrentSnapshot::new(
        cancelled,
        0,
        0,
        vec![spent],
        sync_service::WalletPendingOverlay::default(),
    );
    let observed = wallet
        .owner
        .observe_swap_synced(
            operation,
            cancelled..cancelled + 1,
            Some(sync),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .record()
        .clone();
    assert_eq!(
        state_of(&observed, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
    );
    assert_eq!(filled(first.uid).await, U256::MAX);
    // The cancelled order can't fill, so the retry's pre-hook is unchanged.
    assert_eq!(
        crate::swap_invalidation(&observed, &profile, std::time::SystemTime::now()).unwrap(),
        None
    );

    // The retry signs at k + 1 with the same proof, where the first post-hook is also valid.
    let delegated = wallet.redelegate(operation).await;
    let retry = wallet
        .submit(&fork, delegated, first.note.clone(), None, None)
        .await;

    // Someone funds the executor and runs that older post-hook through another contract, so
    // no direct call to the executor identifies it. The retry's pre-hook is dead, but its
    // order can still fill until `validTo`.
    fork.add_erc20(USDC, executor, first.order.buyAmount).await;
    let post_hook = &first.hooks.post[0];
    let receipt = fork
        .send_as_bypass(
            MULTICALL3,
            IMulticall3::aggregate3Call {
                calls: vec![IMulticall3::Call3 {
                    target: executor,
                    allowFailure: false,
                    callData: post_hook.call_data.clone(),
                }],
            }
            .abi_encode()
            .into(),
            post_hook.gas_limit + 300_000,
        )
        .await;
    assert!(receipt.status(), "the funded older post-hook takes k + 1");
    assert_eq!(fork.erc20_balance(USDC, executor).await, U256::ZERO);
    let stalled_at = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let observed = wallet.observe(operation, stalled_at..stalled_at + 1).await;
    assert_eq!(
        state_of(&observed, 1),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::OlderPostHook)
    );
    assert!(observed.reserved_inputs().contains(&first.input));
    let resolved_at = observed.nonce_watermark().unwrap().block();
    let observed = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1)
        .unwrap()
        .record_synced(operation, None, resolved_at)
        .unwrap();
    assert!(!observed.reserved_inputs().contains(&first.input));
    assert_eq!(filled(retry.uid).await, U256::ZERO);
    let invalidates =
        crate::swap_invalidation(&observed, &profile, std::time::SystemTime::now()).unwrap();
    assert_eq!(invalidates, Some(retry.uid));

    // The next retry is admitted at k + 2 although the older post-hook never won a direct
    // call, and its pre-hook invalidates the stalled order.
    let delegated = wallet.redelegate(operation).await;
    let latest = wallet
        .submit(&fork, delegated, first.note, None, invalidates)
        .await;
    let calls = RelayAdapt7702::executeCall::abi_decode(&latest.hooks.pre[0].call_data)
        .unwrap()
        ._actionData
        .calls;
    assert!(calls.iter().any(|call| {
        call.to == profile.settlement()
            && GPv2Settlement::invalidateOrderCall::abi_decode(&call.data)
                .is_ok_and(|call| call.orderUid[..] == retry.uid.0[..])
    }));

    // The latest pre-hook funds the executor and invalidates the stalled order at once, so a
    // solver can't fill the stalled order with those funds.
    let pre_hook = &latest.hooks.pre[0];
    let receipt = fork
        .send_as_bypass(
            executor,
            pre_hook.call_data.clone(),
            pre_hook.gas_limit + 200_000,
        )
        .await;
    assert!(
        receipt.status(),
        "the latest pre-hook runs before its deadline"
    );
    let funded = receipt.block_number.unwrap();
    assert!(fork.erc20_balance(WETH, executor).await >= latest.order.sellAmount);
    assert_eq!(filled(retry.uid).await, U256::MAX);
    let receipt = fork
        .settle(
            &retry.order,
            retry.signature.clone(),
            &retry.hooks.pre,
            &retry.hooks.post,
        )
        .await;
    assert!(!receipt.status(), "the stalled order can't fill");
    assert!(fork.erc20_balance(WETH, executor).await >= latest.order.sellAmount);

    let receipt = fork
        .settle(
            &latest.order,
            latest.signature.clone(),
            &latest.hooks.pre,
            &latest.hooks.post,
        )
        .await;
    assert!(receipt.status(), "the latest order settles");
    let settled = receipt.block_number.unwrap();
    assert_eq!(filled(latest.uid).await, latest.order.sellAmount);
    assert_eq!(filled(retry.uid).await, U256::MAX);
    fork.mine(wallet.chain.finality_depth).await;
    let observed = wallet.observe(operation, funded..settled + 1).await;
    assert_eq!(state_of(&observed, 2), SwapOrderState::Done);
    assert_eq!(
        state_of(&observed, 1),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::OlderPostHook)
    );
    assert!(
        observed.swap().unwrap().orders()[1]
            .observations()
            .traded
            .is_none()
    );
    drop(store);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_recovery_after_expiry_resets_the_approval_and_skips_dead_pre_hooks() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let profile = wallet.chain.swap_profile().unwrap();
    let early = wallet.swap(&fork).await;
    let unused = wallet.swap(&fork).await;

    let pre_hook = &early.hooks.pre[0];
    let receipt = fork
        .send_as_bypass(
            early.executor,
            pre_hook.call_data.clone(),
            pre_hook.gas_limit + 200_000,
        )
        .await;
    assert!(receipt.status(), "the pre-hook runs before its deadline");
    let ran = receipt.block_number.unwrap();
    fork.increase_time(15 * 60).await;
    fork.mine(wallet.chain.finality_depth + 1).await;
    let confirmed = fork.block_number().await - wallet.chain.finality_depth;
    let record = wallet.observe(early.operation, ran..confirmed + 1).await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::PreHookOnly { expired: true }
    );
    assert!(record.reserved_inputs().contains(&early.input));

    // The expired order can't fill, so the batch resets the approval and shields, at the
    // post-hook's nonce k + 1, without an invalidation.
    let held = fork.erc20_balance(WETH, early.executor).await;
    let prepared = wallet
        .prepare_recovery(early.operation, Some(WETH), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::ONE
        }
    );
    let calls = prepared.calls();
    assert_eq!(calls.len(), 2);
    assert!(resets_approval(&calls[0], WETH, profile.vault_relayer()));
    assert!(shields(&calls[1], early.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the recovery shields the sell token");
    assert_eq!(fork.erc20_balance(WETH, early.executor).await, U256::ZERO);
    assert_eq!(
        fork.call(
            WETH,
            ForkAllowance::allowanceCall {
                owner: early.executor,
                spender: profile.vault_relayer(),
            },
        )
        .await,
        U256::ZERO
    );
    let recovered = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet
        .observe(early.operation, recovered..recovered + 1)
        .await;
    assert_eq!(
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );

    // An expired pre-hook with its nonce k unused is not outstanding: recovery of tokens sent
    // to its executor warns of no competing payload and adds no step for that pre-hook.
    let record = wallet
        .observe(unused.operation, confirmed..confirmed + 1)
        .await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert!(!record.reserved_inputs().contains(&unused.input));
    assert!(!record.has_competing_payloads());
    fork.add_erc20(WETH, unused.executor, U256::from(SELL_AMOUNT))
        .await;
    let prepared = wallet
        .prepare_recovery(unused.operation, Some(WETH), U256::from(SELL_AMOUNT))
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record)
        }
    );
    assert_eq!(prepared.calls().len(), 1);
    assert!(shields(&prepared.calls()[0], unused.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    assert!(deliver(&fork, &issued).await.status());
    assert_eq!(fork.erc20_balance(WETH, unused.executor).await, U256::ZERO);
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_recovery_shields_the_buy_token_a_skipped_post_hook_left() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let swap = wallet.swap(&fork).await;

    let receipt = fork
        .settle(&swap.order, swap.signature.clone(), &swap.hooks.pre, &[])
        .await;
    assert!(receipt.status(), "the solver settles without the post-hook");
    let settled = receipt.block_number.unwrap();
    let held = fork.erc20_balance(USDC, swap.executor).await;
    assert_eq!(held, swap.order.buyAmount);
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::NotDelivered
    );

    // The traded order can't fill again and the settlement used the whole approval, so the
    // batch only shields, competing with the post-hook at k + 1.
    let prepared = wallet
        .prepare_recovery(swap.operation, Some(USDC), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::ONE
        }
    );
    assert_eq!(prepared.calls().len(), 1);
    assert!(shields(&prepared.calls()[0], swap.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the recovery shields the buy token");
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    let recovered = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet
        .observe(swap.operation, recovered..recovered + 1)
        .await;
    assert_eq!(
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_early_cancellation_wins_and_releases_the_inputs_at_finality() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let profile = wallet.chain.swap_profile().unwrap();
    let swap = wallet.swap(&fork).await;
    let record = wallet
        .owner
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == swap.operation)
        .unwrap();

    // A paid execute at the pre-hook's nonce k whose only action is the invalidation.
    let prepared = wallet
        .prepare_recovery(swap.operation, None, U256::ZERO)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record)
        }
    );
    assert!(prepared.shield().is_none());
    assert_eq!(prepared.calls().len(), 1);
    assert!(invalidates(
        &prepared.calls()[0],
        profile.settlement(),
        swap.uid
    ));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the cancellation wins nonce k");
    let cancelled = receipt.block_number.unwrap();
    assert_eq!(
        fork.call(
            profile.settlement(),
            ForkSwapSettlement::filledAmountCall {
                orderUid: swap.uid.0.to_vec().into(),
            },
        )
        .await,
        U256::MAX
    );
    fork.mine(wallet.chain.finality_depth).await;
    // The account read shows nonce k consumed. The order reads as invalidated while the
    // pre-hook's notes are unspent, and that state names the cancellation: no block or
    // receipt of its transaction is read.
    let record = wallet
        .observe(swap.operation, cancelled..cancelled + 1)
        .await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
    );
    assert_eq!(
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
    // Every payload at nonce k keeps its notes until private sync has scanned the block of
    // that read.
    assert!(record.reserved_inputs().contains(&swap.input));
    let resolved_at = record.nonce_watermark().unwrap().block();
    let record = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1)
        .unwrap()
        .record_synced(swap.operation, None, resolved_at)
        .unwrap();
    assert!(!record.reserved_inputs().contains(&swap.input));
    let receipt = fork
        .settle(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
        )
        .await;
    assert!(!receipt.status(), "the cancelled order can't fill");
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_pre_hook_beats_the_cancellation_and_recovery_invalidates_the_order() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let profile = wallet.chain.swap_profile().unwrap();
    let swap = wallet.swap(&fork).await;
    let prepared = wallet
        .prepare_recovery(swap.operation, None, U256::ZERO)
        .await;
    let cancellation = wallet.issue_recovery(&fork, prepared).await;

    // Someone runs the published pre-hook before the cancellation lands.
    let pre_hook = &swap.hooks.pre[0];
    let receipt = fork
        .send_as_bypass(
            swap.executor,
            pre_hook.call_data.clone(),
            pre_hook.gas_limit + 200_000,
        )
        .await;
    assert!(receipt.status(), "the pre-hook takes nonce k");
    let ran = receipt.block_number.unwrap();
    let receipt = deliver(&fork, &cancellation).await;
    assert!(!receipt.status(), "the cancellation loses nonce k");
    let lost = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, ran..lost + 1).await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::PreHookOnly { expired: false }
    );
    // The pre-hook's settlement evidence names what ran. The cancellation's nonce is
    // consumed, so its signature can no longer execute.
    assert_eq!(
        record.payload_state(cancellation.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
    assert!(record.reserved_inputs().contains(&swap.input));

    // Recovery is offered. It invalidates the order that can still fill, resets the approval,
    // and shields the sell token, all in one execute at k + 1.
    let held = fork.erc20_balance(WETH, swap.executor).await;
    let prepared = wallet
        .prepare_recovery(swap.operation, Some(WETH), held)
        .await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::ONE
        }
    );
    let calls = prepared.calls();
    assert_eq!(calls.len(), 3);
    assert!(invalidates(&calls[0], profile.settlement(), swap.uid));
    assert!(resets_approval(&calls[1], WETH, profile.vault_relayer()));
    assert!(shields(&calls[2], swap.executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    assert!(deliver(&fork, &issued).await.status(), "the recovery runs");
    assert_eq!(fork.erc20_balance(WETH, swap.executor).await, U256::ZERO);
    assert_eq!(
        fork.call(
            profile.settlement(),
            ForkSwapSettlement::filledAmountCall {
                orderUid: swap.uid.0.to_vec().into(),
            },
        )
        .await,
        U256::MAX
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
        U256::ZERO
    );
    let receipt = fork
        .settle(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
        )
        .await;
    assert!(!receipt.status(), "the order can't fill after recovery");
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_speed_up_orders_expire_funded_and_recovery_shields_the_sell_token() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let profile = wallet.chain.swap_profile().unwrap();
    let finality = wallet.chain.finality_depth;
    let (first, second) = Box::pin(wallet.overlap(&fork)).await;
    let (operation, executor) = (first.operation, first.executor);

    // The open order's pre-hook runs alone, and no solver fills either order.
    let early = run_pre_hook(&fork, &first).await;
    assert!(
        early.status(),
        "the earlier pre-hook runs before its deadline"
    );
    let ran = early.block_number.unwrap();
    mine_past(
        &fork,
        u64::from(first.order.validTo.max(second.order.validTo)),
    )
    .await;
    fork.mine(finality + 1).await;
    let confirmed = fork.block_number().await - finality;

    // Both orders expired at finalized depth with the funding in the account. Each needs
    // recovery, and neither pre-hook is named as the one that ran.
    let record = wallet.observe(operation, ran..confirmed + 1).await;
    let assert_stranded = |record: &ExecutorRecord| {
        assert_shared_funding(record, [&first, &second], early.transaction_hash);
        for swap in [&first, &second] {
            let order = order_of(record, swap.uid);
            assert!(order.observations().traded.is_none());
            assert_eq!(
                swap_order_state(record.swap().unwrap(), order),
                SwapOrderState::PreHookOnly { expired: true }
            );
        }
        assert!(record.reserved_inputs().contains(&first.input));
    };
    assert_stranded(&record);

    // A restart reads the same record, and observing again changes nothing in it.
    wallet.restart().await;
    assert_eq!(wallet.record(operation), record);
    let record = wallet.observe(operation, confirmed..confirmed + 1).await;
    assert_stranded(&record);

    // Neither expired order can fill, so the batch resets the approval and shields the sell
    // token, at the post-hooks' nonce k + 1, without an invalidation.
    let held = fork.erc20_balance(WETH, executor).await;
    assert_eq!(held, first.order.sellAmount);
    let prepared = wallet.prepare_recovery(operation, Some(WETH), held).await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: pre_hook_nonce(&record) + U256::ONE
        }
    );
    let calls = prepared.calls();
    assert_eq!(calls.len(), 2);
    assert!(resets_approval(&calls[0], WETH, profile.vault_relayer()));
    assert!(shields(&calls[1], executor));
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the recovery shields the sell token");
    // The whole funding goes back to the wallet's private balance and the approval is reset.
    let recovered = shields_of(&receipt, RAILGUN, WETH);
    let [(credited, fee)] = recovered.as_slice() else {
        panic!("the recovery shields the sell token once");
    };
    assert_eq!(*credited + *fee, held);
    assert_eq!(fork.erc20_balance(WETH, executor).await, U256::ZERO);
    assert_eq!(
        relayer_allowance(&fork, &wallet, executor).await,
        U256::ZERO
    );

    // The recovery's nonce resolves, and the record still names no executed pre-hook.
    let recovered = receipt.block_number.unwrap();
    fork.mine(finality).await;
    let record = wallet.observe(operation, recovered..recovered + 1).await;
    assert_eq!(
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
    assert_shared_funding(&record, [&first, &second], early.transaction_hash);
    for swap in [&first, &second] {
        assert!(order_of(&record, swap.uid).observations().traded.is_none());
    }
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_one_cancellation_invalidates_both_overlapping_orders() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let profile = wallet.chain.swap_profile().unwrap();
    let (first, second) = Box::pin(wallet.overlap(&fork)).await;
    let (operation, executor) = (first.operation, first.executor);
    let shared_nonce = pre_hook_nonce(&wallet.record(operation));

    // One paid execute at the shared nonce k, whose only actions invalidate both orders.
    let prepared = wallet.prepare_recovery(operation, None, U256::ZERO).await;
    assert_eq!(
        prepared.execution(),
        ExecutorRecoveryExecution::PaidExecute {
            nonce: shared_nonce
        }
    );
    assert!(prepared.shield().is_none());
    assert_eq!(prepared.calls().len(), 2);
    for swap in [&first, &second] {
        assert!(prepared.calls().iter().any(|call| invalidates(
            call,
            profile.settlement(),
            swap.uid
        )));
    }
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the cancellation wins nonce k");
    let cancelled = receipt.block_number.unwrap();
    for swap in [&first, &second] {
        assert_eq!(order_filled(&fork, &wallet, swap.uid).await, U256::MAX);
    }
    assert_eq!(
        execution_nonce(&fork, executor).await,
        shared_nonce + U256::ONE
    );
    // The shared nonce is consumed, so neither pre-hook can fund the account afterwards.
    for swap in [&first, &second] {
        assert!(
            !run_pre_hook(&fork, swap).await.status(),
            "the cancellation took the pre-hooks' nonce"
        );
    }
    assert_eq!(fork.erc20_balance(WETH, executor).await, U256::ZERO);
    fork.mine(wallet.chain.finality_depth).await;

    // Both orders read as invalidated while the shared notes are unspent, which names the
    // cancellation for each. Nothing funded the account.
    let record = wallet.observe(operation, cancelled..cancelled + 1).await;
    for swap in [&first, &second] {
        assert_eq!(
            swap_order_state(record.swap().unwrap(), order_of(&record, swap.uid)),
            SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
        );
    }
    assert_eq!(record.swap().unwrap().fundings(), [] as [SwapFunding; 0]);
    assert_eq!(
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
    // Every payload at nonce k keeps the shared notes until private sync has scanned the block
    // of that read.
    assert!(record.reserved_inputs().contains(&first.input));
    let resolved_at = record.nonce_watermark().unwrap().block();
    let record = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1)
        .unwrap()
        .record_synced(operation, None, resolved_at)
        .unwrap();
    assert!(!record.reserved_inputs().contains(&first.input));
    for swap in [&first, &second] {
        let receipt = fork
            .settle(
                &swap.order,
                swap.signature.clone(),
                &swap.hooks.pre,
                &swap.hooks.post,
            )
            .await;
        assert!(!receipt.status(), "a cancelled order can't fill");
    }
    wallet.finish().await;
}
