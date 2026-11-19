use super::*;

/// Settle `swap`, an order on an executor whose setup ran, with both hooks, and observe it once
/// the settlement is final. One confirmed read then resolves every setup attempt and both hooks.
async fn fill_at_finality(fork: &ForkChain, wallet: &Wallet, swap: &Swap) {
    let receipt = fork
        .settle(
            &swap.order,
            swap.signature.clone(),
            &swap.hooks.pre,
            &swap.hooks.post,
        )
        .await;
    assert!(receipt.status(), "the solver settles with both hooks");
    assert_eq!(execution_nonce(fork, swap.executor).await, U256::from(3));
    let settled = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, settled..settled + 1).await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::Done
    );
    assert!(record.issued().iter().all(
        |payload| record.payload_state(payload.hash()) == Some(ExecutorPayloadState::Resolved)
    ));
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_order_placed_at_setup_inclusion_fills_and_resolves_at_finality() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let placed = Box::pin(wallet.swap_at_inclusion(&fork)).await;
    fill_at_finality(&fork, &wallet, &placed.swap).await;
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_setup_dropped_by_a_reorg_and_included_again_fills() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let placed = Box::pin(wallet.swap_at_inclusion(&fork)).await;
    wallet.drop_setup(&fork, &placed).await;

    // The same setup lands again on the new chain.
    let receipt = deliver(&fork, &placed.setup).await;
    assert!(receipt.status(), "the setup delegates the executor again");
    fill_at_finality(&fork, &wallet, &placed.swap).await;
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_setup_retried_while_its_order_is_open_lets_the_order_fill() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let placed = Box::pin(wallet.swap_at_inclusion(&fork)).await;
    wallet.drop_setup(&fork, &placed).await;

    // The dropped setup never returns. It is sent again at its nonce next to the open order,
    // paying its fee with a note the order's pre-hook doesn't spend.
    let retry = wallet.retry_setup(placed.swap.operation).await;
    let (issued, _) = wallet
        .issue_setup(&fork, &wallet.owner, &wallet.chain, &retry, &password())
        .await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the retried setup delegates the executor");
    fill_at_finality(&fork, &wallet, &placed.swap).await;
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_order_on_a_setup_dropped_for_good_expires_with_nothing_unshielded() {
    let fork = ForkChain::start().await;
    let wallet = Wallet::open(&fork);
    let placed = Box::pin(wallet.swap_at_inclusion(&fork)).await;
    let swap = &placed.swap;
    wallet.drop_setup(&fork, &placed).await;

    fork.increase_time(15 * 60).await;
    fork.mine(wallet.chain.finality_depth + 1).await;
    let confirmed = fork.block_number().await - wallet.chain.finality_depth;
    let record = wallet
        .observe(swap.operation, confirmed..confirmed + 1)
        .await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );

    // Nothing left the private balance: the order's note is unspent and released, and the
    // account neither holds nor approved any of the sell token.
    let spent = fork
        .call(
            RAILGUN,
            ForkNullifiers::nullifiersCall {
                treeNumber: U256::from(swap.note.tree),
                nullifier: wallet.nullifier(&swap.note),
            },
        )
        .await;
    assert!(!spent);
    assert!(!record.reserved_inputs().contains(&swap.input));
    assert_eq!(fork.erc20_balance(WETH, swap.executor).await, U256::ZERO);
    assert_eq!(
        relayer_allowance(&fork, &wallet, swap.executor).await,
        U256::ZERO
    );
    assert_eq!(order_filled(&fork, &wallet, swap.uid).await, U256::ZERO);

    // The setup stays pending with its fee note reserved, and can be sent again.
    assert_eq!(
        record.payload_state(placed.setup.payload_hash()),
        Some(ExecutorPayloadState::Pending)
    );
    assert!(
        record
            .reserved_inputs()
            .contains(&ExecutorInputIdentity::from_utxo(&placed.setup_fee))
    );
    wallet.retry_setup(swap.operation).await;
    wallet.finish().await;
}
