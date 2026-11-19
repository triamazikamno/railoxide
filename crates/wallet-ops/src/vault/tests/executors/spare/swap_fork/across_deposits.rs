use super::*;

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_across_settlement_deposits_the_approved_terms() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let spoke_pool = wallet.chain.bridge_profile().unwrap().spoke_pool();
    // Each settlement pays a surplus above the order's limit. The post-hook deposits exactly
    // the limit, then shields the surplus to the wallet or leaves it in the account.
    for surplus in [BridgeSurplus::Reshield, BridgeSurplus::KeepInAccount] {
        let reshielded = surplus == BridgeSurplus::Reshield;
        let (swap, receipt, terms) = wallet.settle_across(&fork, surplus).await;
        assert_eq!(swap.order.receiver, swap.executor);
        assert_eq!(
            (terms.spoke_pool, terms.input_token, terms.output_token),
            (spoke_pool, USDC, ARBITRUM_USDC)
        );
        assert_eq!(
            (terms.input_amount, terms.output_amount),
            (swap.order.buyAmount, U256::from(DESTINATION_MINIMUM))
        );

        let deposits = deposits(&receipt, spoke_pool);
        let [deposit] = deposits.as_slice() else {
            panic!("the post-hook deposits once");
        };
        assert_eq!(
            (deposit.depositor, deposit.recipient),
            (
                address_to_bytes32(swap.executor),
                address_to_bytes32(BRIDGE_RECEIVER)
            )
        );
        assert_eq!(
            (
                deposit.inputToken,
                deposit.outputToken,
                deposit.destinationChainId
            ),
            (
                address_to_bytes32(terms.input_token),
                address_to_bytes32(terms.output_token),
                U256::from(ARBITRUM_ONE)
            )
        );
        assert_eq!(
            (deposit.inputAmount, deposit.outputAmount),
            (terms.input_amount, terms.output_amount)
        );
        assert_eq!(
            (deposit.quoteTimestamp, deposit.fillDeadline),
            (terms.quote_timestamp, terms.fill_deadline)
        );
        assert_eq!(
            fork.erc20_balance(USDC, swap.executor).await,
            if reshielded {
                U256::ZERO
            } else {
                U256::from(SURPLUS)
            }
        );

        let record = wallet.record(swap.operation);
        let observed = first_order(&record).observations();
        assert_eq!(
            observed.trade_amounts.map(|amounts| amounts.buy_amount),
            Some(swap.order.buyAmount + U256::from(SURPLUS))
        );
        assert_eq!(
            observed
                .bridge_handoff
                .map(|handoff| (handoff.observation.transaction_hash, handoff.deposit_id)),
            Some((Some(receipt.transaction_hash), Some(deposit.depositId)))
        );
        // Only reshielded surplus is a private credit, net of Railgun's shield fee.
        assert_eq!(
            observed
                .settlement_credit
                .map(|credit| credit.private_amount + credit.fee.unwrap()),
            reshielded.then_some(U256::from(SURPLUS))
        );
        assert_eq!(
            swap_order_state(record.swap().unwrap(), first_order(&record)),
            SwapOrderState::Bridging
        );
    }
    wallet.finish().await;
}

/// The Across deposit terms of `record`'s order `uid`.
fn across_terms(record: &ExecutorRecord, uid: OrderUid) -> AcrossOrderTerms {
    let Some(BridgeOrderTerms::Across(terms)) = order_of(record, uid).bridge().cloned() else {
        panic!("an Across order keeps its deposit terms");
    };
    terms
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_retry_settlement_deposits_an_expired_attempts_across_terms() {
    // The retry is quoted a tenth more on the destination chain than the first attempt.
    let lower = U256::from(DESTINATION_MINIMUM);
    let higher = lower + lower / U256::from(10);
    // Each choice gets its own fork, since the scenario moves the fork's clock.
    for (surplus, label) in SURPLUS_CHOICES {
        let fork = ForkChain::start().await;
        let mut wallet = Wallet::open(&fork);
        let spoke_pool = wallet.chain.bridge_profile().unwrap().spoke_pool();
        let finality = wallet.chain.finality_depth;
        let reshielded = surplus == BridgeSurplus::Reshield;
        wallet.pair = (USDC, across_delivery(surplus));
        let first = wallet.swap(&fork).await;
        let (operation, executor) = (first.operation, first.executor);

        // The first attempt expires with its pre-hook's nonce k unused.
        mine_past(&fork, u64::from(first.order.validTo)).await;
        fork.mine(finality + 1).await;
        let confirmed = fork.block_number().await - finality;
        let record = wallet.observe(operation, confirmed..confirmed + 1).await;
        assert_eq!(
            swap_order_state(record.swap().unwrap(), order_of(&record, first.uid)),
            SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired),
            "{label}"
        );
        let (pre_nonce, post_nonce) = hook_nonces(&record, first.uid);
        assert_eq!(post_nonce, pre_nonce + U256::ONE);
        assert_eq!(execution_nonce(&fork, executor).await, pre_nonce);

        // The retry keeps the pair and the delivery, and signs at k again. Its `validTo` comes
        // from the local clock, which the fork's clock is now ahead of, so it is valid for ten
        // minutes of the fork's clock.
        let delegated = wallet.redelegate(operation).await;
        let ahead = fork
            .timestamp()
            .await
            .saturating_sub(u64::from(valid_until(0)));
        // An ordinary retry discloses no continuing minimum, so it signs without accepting
        // the first attempt's lower one.
        let SwapAmountPlan::Fits(plan) = wallet.plan(
            delegated,
            std::slice::from_ref(&first.note),
            U256::from(SELL_AMOUNT),
            None,
        ) else {
            panic!("one note fits one order");
        };
        let Ok(retry) = wallet
            .sign_with(
                &fork,
                plan,
                std::slice::from_ref(&first.note),
                None,
                Signing {
                    destination_minimum: higher,
                    continuing_minimum: None,
                    valid_for: std::time::Duration::from_secs(ahead + 10 * 60),
                    minimum_discount: U256::ZERO,
                },
            )
            .await
        else {
            panic!("{label}: the retry is submitted without accepting a continuing minimum");
        };
        let record = wallet.record(operation);
        let (expired_terms, retry_terms) = (
            across_terms(&record, first.uid),
            across_terms(&record, retry.uid),
        );
        assert_eq!(
            (expired_terms.output_amount, retry_terms.output_amount),
            (lower, higher)
        );
        assert!(retry_terms.quote_timestamp > expired_terms.quote_timestamp);
        assert_eq!(hook_nonces(&record, retry.uid), (pre_nonce, post_nonce));

        // One settlement runs the retry's pre-hook and pays its trade to the account, then
        // runs the expired attempt's post-hook where the retry's would run.
        let payout = retry.order.buyAmount + U256::from(SURPLUS);
        let left = payout
            .checked_sub(expired_terms.input_amount)
            .expect("the payout covers the expired attempt's deposit");
        assert!(!left.is_zero());
        let receipt = fork
            .settle_with_surplus(
                &retry.order,
                retry.signature.clone(),
                &retry.hooks.pre,
                &first.hooks.post,
                U256::from(SURPLUS),
            )
            .await;
        assert!(receipt.status(), "{label}: the solver settles the retry");

        // The one deposit carries the expired attempt's signed terms, not the retry's.
        let deposits = deposits(&receipt, spoke_pool);
        let [deposit] = deposits.as_slice() else {
            panic!("{label}: the expired attempt's post-hook deposits once");
        };
        assert_eq!(
            (
                deposit.depositor,
                deposit.recipient,
                deposit.destinationChainId
            ),
            (
                address_to_bytes32(executor),
                address_to_bytes32(BRIDGE_RECEIVER),
                U256::from(ARBITRUM_ONE)
            )
        );
        assert_eq!(
            (deposit.inputAmount, deposit.outputAmount),
            (expired_terms.input_amount, lower)
        );
        assert_eq!(
            (deposit.quoteTimestamp, deposit.fillDeadline),
            (expired_terms.quote_timestamp, expired_terms.fill_deadline)
        );
        // The post-hook shields what the deposit left to the wallet, or leaves it in the
        // account.
        let shields = shields_of(&receipt, RAILGUN, USDC);
        let shield = if reshielded {
            let [(credited, fee)] = shields.as_slice() else {
                panic!("the post-hook shields the surplus once");
            };
            assert_eq!(*credited + *fee, left);
            Some((*credited, Some(*fee)))
        } else {
            assert_eq!(shields, [] as [(U256, U256); 0]);
            None
        };
        assert_eq!(
            fork.erc20_balance(USDC, executor).await,
            if reshielded { U256::ZERO } else { left }
        );
        assert_eq!(
            execution_nonce(&fork, executor).await,
            post_nonce + U256::ONE
        );

        // The settlement is observed for the retry. The trade stays on the retry, and the
        // hand-off, the delivery and the credit go to the attempt whose terms were deposited.
        let settled = receipt.block_number.unwrap();
        fork.mine(finality).await;
        wallet
            .owner
            .observe_swap_settlement(
                operation,
                retry.uid,
                settled,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        let record = wallet.record(operation);
        let (expired_order, retry_order) =
            (order_of(&record, first.uid), order_of(&record, retry.uid));
        let (delivered, traded) = (expired_order.observations(), retry_order.observations());
        let trade = traded.traded.expect("the retry traded");
        assert_eq!(trade.transaction_hash, Some(receipt.transaction_hash));
        assert_eq!(
            traded.trade_amounts.map(|amounts| amounts.buy_amount),
            Some(payout)
        );
        assert_eq!(
            (
                traded.bridge_handoff,
                traded.delivered,
                traded.settlement_credit
            ),
            (None, None, None)
        );
        assert_eq!(
            delivered.bridge_handoff,
            Some(SwapBridgeHandoff {
                observation: trade,
                deposit_id: Some(deposit.depositId),
            })
        );
        assert_eq!((delivered.delivered, delivered.traded), (Some(trade), None));
        assert_eq!(
            delivered
                .settlement_credit
                .map(|credit| (credit.private_amount, credit.fee)),
            shield
        );
        assert_eq!(
            record
                .swap_bridges_to_track()
                .map(SwapOrderRecord::uid)
                .collect::<Vec<_>>(),
            vec![first.uid]
        );
        let recorded = record.swap().unwrap();
        assert_eq!(
            (
                recorded
                    .trade_order(expired_order)
                    .map(SwapOrderRecord::uid),
                recorded
                    .delivery_order(retry_order)
                    .map(SwapOrderRecord::uid)
            ),
            (Some(retry.uid), Some(first.uid))
        );
        for order in [expired_order, retry_order] {
            assert_eq!(
                crate::swap_attempt_state(&record, order),
                SwapOrderState::Bridging
            );
        }

        // Reconciliation names the expired attempt's post-hook as the one that deposited: only
        // its terms match. The retry is neither undelivered nor ended.
        let record = wallet.observe(operation, settled..settled + 1).await;
        let (delivered, traded) = (
            order_of(&record, first.uid).observations(),
            order_of(&record, retry.uid).observations(),
        );
        assert_eq!(delivered.post_hook_deposit, Some(trade));
        assert_eq!((traded.post_hook_deposit, traded.undelivered), (None, None));
        assert!(traded.pre_hook_dead.is_none());
        wallet.finish().await;
    }
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_open_order_settlement_deposits_its_speed_ups_across_terms() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let spoke_pool = wallet.chain.bridge_profile().unwrap().spoke_pool();
    let finality = wallet.chain.finality_depth;
    wallet.pair = (USDC, across_delivery(BridgeSurplus::Reshield));
    let (first, second) = Box::pin(wallet.overlap(&fork)).await;
    let (operation, executor) = (first.operation, first.executor);

    // Both attempts are live, at the same hook nonces. The speed-up deposits less for the same
    // destination minimum, so its deposit can't be taken for the open order's.
    let record = wallet.record(operation);
    let (open_terms, speed_up_terms) = (
        across_terms(&record, first.uid),
        across_terms(&record, second.uid),
    );
    assert_eq!(
        (open_terms.input_amount, speed_up_terms.input_amount),
        (first.order.buyAmount, second.order.buyAmount)
    );
    assert_eq!(speed_up_terms.output_amount, open_terms.output_amount);
    let (pre_nonce, post_nonce) = hook_nonces(&record, first.uid);
    assert_eq!(execution_nonce(&fork, executor).await, pre_nonce);

    // One settlement runs the open order's pre-hook and pays its trade to the account, then
    // runs the speed-up's post-hook where the open order's would run.
    let payout = first.order.buyAmount + U256::from(SURPLUS);
    let left = payout
        .checked_sub(speed_up_terms.input_amount)
        .expect("the payout covers the speed-up's deposit");
    let receipt = fork
        .settle_with_surplus(
            &first.order,
            first.signature.clone(),
            &first.hooks.pre,
            &second.hooks.post,
            U256::from(SURPLUS),
        )
        .await;
    assert!(receipt.status(), "the solver settles the open order");
    assert_eq!(
        order_filled(&fork, &wallet, first.uid).await,
        first.order.sellAmount
    );
    assert_eq!(order_filled(&fork, &wallet, second.uid).await, U256::ZERO);

    // The one deposit carries the speed-up's signed terms, not the traded order's.
    let deposits = deposits(&receipt, spoke_pool);
    let [deposit] = deposits.as_slice() else {
        panic!("the speed-up's post-hook deposits once");
    };
    assert_eq!(
        (
            deposit.depositor,
            deposit.recipient,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(executor),
            address_to_bytes32(BRIDGE_RECEIVER),
            U256::from(ARBITRUM_ONE)
        )
    );
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (speed_up_terms.input_amount, speed_up_terms.output_amount)
    );
    assert_eq!(
        (deposit.quoteTimestamp, deposit.fillDeadline),
        (speed_up_terms.quote_timestamp, speed_up_terms.fill_deadline)
    );
    // The post-hook shields what the deposit left to the wallet.
    let shields = shields_of(&receipt, RAILGUN, USDC);
    let [(credited, fee)] = shields.as_slice() else {
        panic!("the post-hook shields the surplus once");
    };
    assert_eq!(*credited + *fee, left);
    assert_eq!(fork.erc20_balance(USDC, executor).await, U256::ZERO);
    assert_eq!(
        execution_nonce(&fork, executor).await,
        post_nonce + U256::ONE
    );

    // The settlement is confirmed from its receipts for the open order. The trade stays on
    // it, and the hand-off, the delivery and the credit go to the speed-up, whose terms were
    // deposited in that same receipt.
    let settled = receipt.block_number.unwrap();
    fork.mine(finality).await;
    wallet
        .owner
        .observe_swap_settlement(
            operation,
            first.uid,
            settled,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let record = wallet.record(operation);
    let assert_paired = |record: &ExecutorRecord| {
        let (traded_order, delivery_order) =
            (order_of(record, first.uid), order_of(record, second.uid));
        let (traded, delivered) = (traded_order.observations(), delivery_order.observations());
        let trade = traded.traded.expect("the open order traded");
        assert_eq!(trade.transaction_hash, Some(receipt.transaction_hash));
        assert_eq!(
            traded.trade_amounts.map(|amounts| amounts.buy_amount),
            Some(payout)
        );
        assert_eq!(
            (
                traded.bridge_handoff,
                traded.delivered,
                traded.settlement_credit
            ),
            (None, None, None)
        );
        assert_eq!(
            delivered.bridge_handoff,
            Some(SwapBridgeHandoff {
                observation: trade,
                deposit_id: Some(deposit.depositId),
            })
        );
        assert_eq!((delivered.delivered, delivered.traded), (Some(trade), None));
        assert_eq!(
            delivered
                .settlement_credit
                .map(|credit| (credit.private_amount, credit.fee)),
            Some((*credited, Some(*fee)))
        );
        // The pairing stays within the one swap use both attempts belong to.
        assert_eq!(
            (traded_order.use_id(), delivery_order.use_id()),
            (Some(first.swap_use), Some(first.swap_use))
        );
        let recorded = record.swap().unwrap();
        assert_eq!(
            (
                recorded
                    .delivery_order(traded_order)
                    .map(SwapOrderRecord::uid),
                recorded
                    .trade_order(delivery_order)
                    .map(SwapOrderRecord::uid)
            ),
            (Some(second.uid), Some(first.uid))
        );
        assert_eq!(
            record
                .swap_bridges_to_track()
                .map(SwapOrderRecord::uid)
                .collect::<Vec<_>>(),
            vec![second.uid]
        );
        for order in [traded_order, delivery_order] {
            assert_eq!(
                crate::swap_attempt_state(record, order),
                SwapOrderState::Bridging
            );
        }
    };
    assert_paired(&record);

    // The pairing is read back from a reopened store and by a restarted owner.
    let reopened = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1)
        .unwrap()
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(reopened, record);
    wallet.restart().await;
    assert_paired(&wallet.record(operation));

    // Reconciliation keeps the pairing and names the speed-up's post-hook as the one that
    // deposited: only its terms match. The traded order is neither undelivered nor ended.
    let record = wallet.observe(operation, settled..settled + 1).await;
    assert_paired(&record);
    let (traded, delivered) = (
        order_of(&record, first.uid).observations(),
        order_of(&record, second.uid).observations(),
    );
    assert_eq!(delivered.post_hook_deposit, traded.traded);
    assert_eq!((traded.post_hook_deposit, traded.undelivered), (None, None));
    assert!(traded.pre_hook_dead.is_none());
    wallet.finish().await;
}

/// The next block of `fork` is mined at exactly `timestamp`, Unix seconds.
async fn set_next_block_time(fork: &ForkChain, timestamp: u64) {
    use alloy::providers::{DynProvider, Provider as _, ProviderBuilder};
    let provider: DynProvider = ProviderBuilder::default().connect_http(fork.url()).erased();
    let _: serde_json::Value = provider
        .raw_request("evm_setNextBlockTimestamp".into(), (U256::from(timestamp),))
        .await
        .unwrap_or_else(|error| panic!("evm_setNextBlockTimestamp: {error}"));
}

/// `DEPOSITOR`'s own `depositV3` of `terms` to `BRIDGE_RECEIVER` on Arbitrum One, with the
/// quote dated `quote_timestamp` and `fill_deadline`, sent in a block mined at exactly `at`.
async fn deposit_at(
    fork: &ForkChain,
    terms: &AcrossOrderTerms,
    quote_timestamp: u64,
    fill_deadline: u64,
    at: u64,
) -> TransactionReceipt {
    let deposit = SpokePool::depositV3Call {
        depositor: DEPOSITOR,
        recipient: BRIDGE_RECEIVER,
        inputToken: terms.input_token,
        outputToken: terms.output_token,
        inputAmount: terms.input_amount,
        outputAmount: terms.output_amount,
        destinationChainId: U256::from(ARBITRUM_ONE),
        exclusiveRelayer: terms.exclusive_relayer,
        quoteTimestamp: u32::try_from(quote_timestamp).unwrap(),
        fillDeadline: u32::try_from(fill_deadline).unwrap(),
        exclusivityParameter: terms.exclusivity_parameter,
        message: Bytes::new(),
    };
    set_next_block_time(fork, at).await;
    let receipt = fork
        .send(
            TransactionRequest::default()
                .from(DEPOSITOR)
                .to(terms.spoke_pool)
                .input(deposit.abi_encode().into())
                .gas_limit(1_000_000),
        )
        .await;
    assert_eq!(fork.timestamp().await, at, "the deposit's block time");
    receipt
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_spoke_pool_rejects_a_deposit_past_its_quote_age() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let spoke_pool = wallet.chain.bridge_profile().unwrap().spoke_pool();
    let finality = wallet.chain.finality_depth;
    let quote_buffer = u64::from(
        fork.call(
            spoke_pool,
            ForkSpokePoolBuffers::depositQuoteTimeBufferCall {},
        )
        .await,
    );
    let fill_buffer = u64::from(
        fork.call(spoke_pool, ForkSpokePoolBuffers::fillDeadlineBufferCall {})
            .await,
    );
    assert!(quote_buffer > 0 && fill_buffer > 4);
    eprintln!(
        "SpokePool depositQuoteTimeBuffer: {quote_buffer} s, fillDeadlineBuffer: {fill_buffer} s"
    );

    // An Across attempt whose quote is dated at the fork's latest block. Its pre-hook never
    // runs, so the account's nonce stays below its post-hook's.
    let delivery = across_delivery(BridgeSurplus::KeepInAccount);
    wallet.pair = (USDC, delivery.clone());
    let swap = wallet.swap(&fork).await;
    let record = wallet.record(swap.operation);
    let terms = across_terms(&record, swap.uid);
    assert_eq!(terms.spoke_pool, spoke_pool);
    let (pre_nonce, post_nonce) = hook_nonces(&record, swap.uid);
    let quoted = u64::from(terms.quote_timestamp);
    // The last block time at which the pool still takes that quote.
    let boundary = quoted + quote_buffer;
    // Not before any block below, and within the pool's fill deadline buffer of each.
    let fill_deadline = boundary + fill_buffer / 2;

    fork.impersonate(DEPOSITOR).await;
    fork.add_erc20(USDC, DEPOSITOR, terms.input_amount * U256::from(3))
        .await;
    let approved = fork
        .send(
            TransactionRequest::default()
                .from(DEPOSITOR)
                .to(USDC)
                .input(
                    approveCall {
                        spender: spoke_pool,
                        amount: U256::MAX,
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_limit(200_000),
        )
        .await;
    assert!(approved.status(), "the depositor approves the SpokePool");

    // A quote exactly the buffer old is taken, and one second later it reverts.
    let taken = deposit_at(&fork, &terms, quoted, fill_deadline, boundary).await;
    assert!(taken.status(), "a quote exactly the buffer old is taken");
    let deposits_taken = deposits(&taken, spoke_pool);
    let [deposit] = deposits_taken.as_slice() else {
        panic!("the pool takes one deposit");
    };
    assert_eq!(deposit.quoteTimestamp, terms.quote_timestamp);
    let rejected = deposit_at(&fork, &terms, quoted, fill_deadline, boundary + 1).await;
    assert!(!rejected.status(), "a quote past the buffer reverts");
    assert!(deposits(&rejected, spoke_pool).is_empty());
    // One block later still, a quote again exactly the buffer old is taken with every other
    // argument unchanged. The fill deadline's lower bound only tightens with time and its
    // upper bound only loosens, so both held in the rejected block between the two deposits
    // that were taken. Only the quote's age rejected it.
    let later = deposit_at(&fork, &terms, quoted + 2, fill_deadline, boundary + 2).await;
    assert!(later.status(), "the later quote is taken a block later");
    assert_eq!(deposits(&later, spoke_pool).len(), 1);

    // The wallet's quote-age rule in `continuing.rs` reads the same boundary from one
    // finalized block: the approval continues at the boundary block and is excluded at the
    // next one, with the post-hook's nonce unused at both.
    let boundary_block = taken.block_number.unwrap();
    assert_eq!(rejected.block_number.unwrap(), boundary_block + 1);
    assert_eq!(execution_nonce(&fork, swap.executor).await, pre_nonce);
    assert!(pre_nonce < post_nonce);
    let continuing = || async {
        wallet
            .owner
            .swap_continuing_approvals(
                swap.operation,
                swap.swap_use,
                &delivery,
                Some(pre_nonce),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap()
            .approvals()
            .iter()
            .map(|approval| approval.uid)
            .collect::<Vec<_>>()
    };
    fork.mine(boundary_block + finality - fork.block_number().await)
        .await;
    assert_eq!(continuing().await, vec![swap.uid]);
    fork.mine(1).await;
    assert_eq!(continuing().await, [] as [OrderUid; 0]);
    wallet.finish().await;
}

/// `count` notes in separate trees, each spent by its own pre-hook transaction. Their orders are
/// signed but never settled, so the trees' roots don't matter.
fn separate_tree_notes(wallet: &Wallet, count: u16) -> Vec<Utxo> {
    (0..count)
        .map(|number| {
            wallet.note(
                RailgunTree {
                    number,
                    root: B256::ZERO,
                },
                U256::from(2 * SELL_AMOUNT),
            )
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_across_order_app_data_stays_within_the_planned_length() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    // Reshielded surplus gives an Across order its largest post-hook.
    wallet.pair = (USDC, across_delivery(BridgeSurplus::Reshield));
    let budget = wallet.chain.swap_profile().unwrap().app_data_byte_budget();

    for (count, amount) in [(1, SELL_AMOUNT), (2, 3 * SELL_AMOUNT)] {
        let delegated = wallet.delegate(&fork).await;
        let notes = separate_tree_notes(&wallet, count);
        let SwapAmountPlan::Fits(plan) = wallet.plan(delegated, &notes, U256::from(amount), None)
        else {
            panic!("{count} transactions fit one order");
        };
        assert_eq!(plan.transaction_count(), usize::from(count));
        let planned = plan.app_data_len();
        let (inputs, transactions) = wallet.synthetic_pre_hook(&plan, &notes);
        let signed = wallet
            .sign(&fork, plan, &inputs, Some(transactions))
            .await
            .app_data_len;
        assert!(
            signed <= planned && planned <= budget,
            "{count} transactions: signed {signed}, planned {planned}, budget {budget}"
        );
        eprintln!("Across app data, {count} transactions: signed {signed}, planned {planned}");
    }

    // An amount that needs nine transactions, one more than a batch allows. The largest amount
    // offered instead fits the budget, and so does its signed order.
    let delegated = wallet.delegate(&fork).await;
    let notes = separate_tree_notes(&wallet, 9);
    let SwapAmountPlan::TooLarge { largest } =
        wallet.plan(delegated, &notes, U256::from(18 * SELL_AMOUNT), None)
    else {
        panic!("nine transactions don't fit one order");
    };
    let (count, planned) = (largest.transaction_count(), largest.app_data_len());
    let (inputs, transactions) = wallet.synthetic_pre_hook(&largest, &notes);
    let signed = wallet
        .sign(&fork, largest, &inputs, Some(transactions))
        .await
        .app_data_len;
    assert!(
        signed <= planned && planned <= budget,
        "largest offer, {count} transactions: signed {signed}, planned {planned}, budget {budget}"
    );
    eprintln!(
        "Across app data, largest offer of {count} transactions: signed {signed}, planned {planned}"
    );
    wallet.finish().await;
}

// A private Across order's post-hook also carries the handler message, which the plan sizes from
// a placeholder of the destination account's shield.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_order_app_data_stays_within_the_planned_length() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let mut wallet = Wallet::open(&fork).with_destination(&destination_fork);
    let budget = wallet.chain.swap_profile().unwrap().app_data_byte_budget();

    for (surplus, label) in SURPLUS_CHOICES {
        // One pre-hook transaction, then an amount that needs nine, one more than a batch
        // allows, for the largest amount offered instead.
        for (note_count, amount) in [(1, SELL_AMOUNT), (9, 18 * SELL_AMOUNT)] {
            let delegated = wallet
                .delegate_private(
                    &fork,
                    &destination_fork,
                    surplus,
                    BridgeShieldFailure::RefundOnOrigin,
                )
                .await;
            let notes = separate_tree_notes(&wallet, note_count);
            let plan = match wallet.try_plan(delegated, &notes, U256::from(amount), None) {
                Ok(SwapAmountPlan::Fits(plan)) if notes.len() == 1 => plan,
                Ok(SwapAmountPlan::TooLarge { largest }) if notes.len() == 9 => largest,
                unplanned => {
                    // Not even this order's smallest pre-hook fits beside the handler message.
                    // Only a reshielded surplus, the larger post-hook, may leave no room.
                    let reported = match &unplanned {
                        Ok(SwapAmountPlan::Fits(_)) => "fits".to_owned(),
                        Ok(SwapAmountPlan::TooLarge { .. }) => "too large".to_owned(),
                        Err(error) => error.to_string(),
                    };
                    assert!(
                        surplus == BridgeSurplus::Reshield,
                        "{} notes with surplus kept in the account: {reported}",
                        notes.len()
                    );
                    println!(
                        "MEASURE private_across_app_data surplus={label} notes={} fits=false planner={reported:?}",
                        notes.len()
                    );
                    continue;
                }
            };
            let (count, planned) = (plan.transaction_count(), plan.app_data_len());
            let (inputs, transactions) = wallet.synthetic_pre_hook(&plan, &notes);
            let signed = wallet
                .sign(&fork, plan, &inputs, Some(transactions))
                .await
                .app_data_len;
            assert!(
                signed <= planned && planned <= budget,
                "surplus {label}, {count} transactions: signed {signed}, planned {planned}, budget {budget}"
            );
            println!(
                "MEASURE private_across_app_data surplus={label} transactions={count} signed={signed} planned={planned} budget={budget}"
            );
        }
    }
    wallet.finish().await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_refunded_across_deposit_is_recovered_to_the_wallet() {
    let fork = ForkChain::start().await;
    let mut wallet = Wallet::open(&fork);
    let (swap, receipt, terms) = wallet.settle_across(&fork, BridgeSurplus::Reshield).await;
    let settled = receipt.block_number.unwrap();
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);

    // The deposit expires unfilled, and the SpokePool refunds its input to the depositor.
    fork.impersonate(terms.spoke_pool).await;
    let refund = fork
        .send(
            alloy::rpc::types::TransactionRequest::default()
                .from(terms.spoke_pool)
                .to(USDC)
                .input(
                    ForkTransfer::transferCall {
                        to: swap.executor,
                        amount: terms.input_amount,
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_limit(200_000),
        )
        .await;
    assert!(refund.status(), "the SpokePool refunds the deposit");
    let refunded = refund.block_number.unwrap();
    let store = ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), 1).unwrap();
    let record = store
        .record_swap_bridge_outcome(swap.operation, swap.uid, SwapBridgeOutcome::Refunding)
        .unwrap();
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::Refunding
    );

    // The explicit check reconciles the account at a final block that holds the refund.
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet.observe(swap.operation, settled..refunded + 1).await;
    assert_eq!(
        swap_order_state(record.swap().unwrap(), first_order(&record)),
        SwapOrderState::Refunding
    );
    let held = fork.erc20_balance(USDC, swap.executor).await;
    assert_eq!(held, terms.input_amount);

    // Both hooks used their nonces and the traded order can't fill again, so the batch only
    // shields the refund, at k + 2.
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
    let npk = prepared.shield().unwrap().preimage.npk;
    let issued = wallet.issue_recovery(&fork, prepared).await;
    let receipt = deliver(&fork, &issued).await;
    assert!(receipt.status(), "the recovery shields the refund");
    assert_eq!(fork.erc20_balance(USDC, swap.executor).await, U256::ZERO);
    // The wallet's new note holds the refund less Railgun's shield fee.
    let credits = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == RAILGUN)
        .filter_map(|log| log.log_decode::<Shield>().ok())
        .flat_map(|log| {
            let event = log.inner.data;
            event.commitments.into_iter().zip(event.fees)
        })
        .filter(|(preimage, _)| preimage.npk == npk)
        .map(|(preimage, fee)| (U256::from(preimage.value), fee))
        .collect::<Vec<_>>();
    let [(credited, fee)] = credits.as_slice() else {
        panic!("the recovery shields once to the wallet");
    };
    assert!(!fee.is_zero());
    assert_eq!(*credited + *fee, held);
    let recovered = receipt.block_number.unwrap();
    fork.mine(wallet.chain.finality_depth).await;
    let record = wallet
        .observe(swap.operation, recovered..recovered + 1)
        .await;
    assert_eq!(
        record.payload_state(issued.payload_hash()),
        Some(ExecutorPayloadState::Resolved)
    );
    drop(store);
    wallet.finish().await;
}
