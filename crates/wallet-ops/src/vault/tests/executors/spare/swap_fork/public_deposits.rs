use super::*;

// Ethereum's USDT refuses a change from one nonzero allowance to another. A Public account
// that already allows the `SpokePool` 100 and sells 200 resets the allowance first, and both
// approvals cost no more gas than the plan the review showed.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL and anvil"]
async fn swap_fork_public_usdt_approval_resets_a_short_allowance_first() {
    use alloy::primitives::{address, keccak256};
    use broadcaster_core::contracts::shield::build_approve_calldata;

    const USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");
    const MAX_FEE_PER_GAS: u128 = 200_000_000_000;
    const MAX_PRIORITY_FEE_PER_GAS: u128 = 2_000_000_000;

    let fork = ForkChain::start().await;
    // The swap's record is on the destination chain's owner, which reads nothing of its own
    // chain here. Every read and transaction goes to the origin fork.
    let wallet = Wallet::open(&fork).with_destination(&fork);
    let origin = &wallet.chain;
    let owner = &wallet.destination.as_ref().unwrap().owner;
    let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
    let key = keccak256(format!(
        "{:?} {}",
        std::time::SystemTime::now(),
        std::process::id()
    ));
    let signer = VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key(key.0).unwrap());
    let source = signer.address();
    let (sell_amount, allowed) = (U256::from(200), U256::from(100));
    let allowance = || ForkAllowance::allowanceCall {
        owner: source,
        spender: spoke_pool,
    };

    // The account holds USDT and gas money, and already allows the pool less than it sells.
    fork.impersonate(source).await;
    fork.add_erc20(USDT, source, U256::from(1_000)).await;
    let receipt = fork
        .send(
            TransactionRequest::default()
                .from(source)
                .to(USDT)
                .input(Bytes::from(build_approve_calldata(spoke_pool, allowed)).into())
                .gas_limit(100_000),
        )
        .await;
    assert!(receipt.status());
    assert_eq!(fork.call(USDT, allowance()).await, allowed);

    let plan = owner
        .plan_public_swap_gas(
            origin,
            source,
            USDT,
            spoke_pool,
            sell_amount,
            true,
            MAX_FEE_PER_GAS,
            MAX_PRIORITY_FEE_PER_GAS,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(plan.approval_gas_limits.len(), 2);

    // The smallest claim of a direct USDT deposit from this account, approved for the plan.
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let mut bounds = setup_approval(USDT, WETH, SwapDelivery::Reshield).bounds;
    bounds.sell_amount = sell_amount;
    bounds.destination_minimum = Some(U256::from(100));
    bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
    bounds.destination_setup_fee = Some(setup_fee_limit(1_000));
    owner
        .claim_public_swap(PublicSwapUseClaim {
            id,
            origin_chain: 1,
            source,
            source_scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: TEST_WALLET_ID.to_owned(),
            },
            account: SwapAccountChoice::New(operation),
            destination_token: DESTINATION_TOKEN,
            intent: PublicSwapIntent {
                bridged_token: USDT,
                order: false,
            },
            approval: PublicSwapApproval {
                bounds,
                price_verified: Some(true),
                price_acknowledged: false,
                sell_token: USDT,
                on_shield_failure: BridgeShieldFailure::KeepOnDestination,
                destination: SwapApprovedAccount {
                    address: None,
                    setup: true,
                },
                max_gas_cost: plan.max_gas_cost,
            },
        })
        .unwrap();

    let funded = fork.native_balance(source).await;
    owner
        .submit_public_swap_approvals_with_signer(
            operation,
            id,
            origin,
            &signer,
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: MAX_FEE_PER_GAS,
                max_priority_fee_per_gas: MAX_PRIORITY_FEE_PER_GAS,
            },
            crate::OperationHttpClient::direct_for_tests(),
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(fork.call(USDT, allowance()).await, sell_amount);

    // approve(0), then approve(200), both included and successful.
    let store =
        ExecutorStore::new(wallet.db.clone(), wallet.view.clone(), DESTINATION_CHAIN).unwrap();
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let (_, swap) = record.public_swap_use(id).unwrap();
    let [reset, approval] = swap.transactions() else {
        panic!("a reset and an approval were sent");
    };
    for (transaction, kind, value) in [
        (reset, PublicSwapTransactionKind::ApprovalReset, U256::ZERO),
        (approval, PublicSwapTransactionKind::Approval, sell_amount),
    ] {
        assert_eq!(transaction.kind, kind);
        assert_eq!(
            transaction.transaction.input.input().unwrap()[..],
            build_approve_calldata(spoke_pool, value)[..]
        );
        assert!(transaction.inclusion.unwrap().succeeded);
    }

    // The approvals carry no value, so what the account lost is the gas both receipts paid.
    let paid = funded - fork.native_balance(source).await;
    assert!(!paid.is_zero());
    assert!(
        paid <= plan.max_gas_cost,
        "the approvals paid {paid} wei of the planned {} wei",
        plan.max_gas_cost
    );
    println!(
        "MEASURE public_usdt_approvals_gas paid={paid} planned={}",
        plan.max_gas_cost
    );
    drop(store);
    wallet.finish().await;
}

/// Send `swap`'s deposit from its Public account through the wallet. The pinned `SpokePool`
/// must take one deposit of the signed amounts that names the Public account as depositor and
/// the destination chain's handler as recipient, with the signed message, for no more gas than
/// the plan the swap was approved with. Returns the deposit's receipt and event.
async fn deposit_from_public_account(
    wallet: &Wallet,
    fork: &ForkChain,
    signer: &VaultedPublicSigner,
    swap: &PublicSwap,
    signed: &SignedDelivery,
) -> (TransactionReceipt, SpokePool::FundsDeposited) {
    let destination = wallet.destination.as_ref().unwrap();
    let origin = &wallet.chain;
    let outcome = destination
        .owner
        .submit_public_swap_deposit_with_signer(
            swap.operation,
            swap.id,
            origin,
            signer,
            PUBLIC_GAS_FEE,
            &signed.delivery,
            &signed.terms,
            crate::OperationHttpClient::direct_for_tests(),
            &mut |_| {},
        )
        .await
        .unwrap();
    let PublicSwapTransactionOutcome::Included {
        transaction_hash, ..
    } = outcome
    else {
        panic!("the deposit succeeds: {outcome:?}");
    };
    let receipt = fork.receipt(transaction_hash).await;
    let deposits = deposits(
        &receipt,
        origin.bridge_origin_profile().unwrap().spoke_pool(),
    );
    let [deposit] = deposits.as_slice() else {
        panic!("the Public account deposits once");
    };
    let handler = destination
        .chain
        .bridge_profile()
        .unwrap()
        .multicall_handler();
    assert_eq!(
        (deposit.depositor, deposit.recipient),
        (address_to_bytes32(swap.source), address_to_bytes32(handler))
    );
    assert_eq!(deposit.message, signed.message);
    assert_eq!(
        (
            deposit.inputToken,
            deposit.outputToken,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(swap.terms.bridged_token),
            address_to_bytes32(swap.terms.destination_token),
            U256::from(destination.chain.chain_id)
        )
    );
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (swap.terms.sell_amount, swap.terms.destination_minimum)
    );
    assert_eq!(
        (signed.terms.input_amount, signed.terms.output_amount),
        (deposit.inputAmount, deposit.outputAmount)
    );
    assert_eq!(
        (deposit.quoteTimestamp, deposit.fillDeadline),
        (signed.terms.quote_timestamp, signed.terms.fill_deadline)
    );
    let limit = swap.plan.deposit_gas_limit.unwrap();
    assert!(
        receipt.gas_used <= limit,
        "the deposit used {} gas of the planned {limit}",
        receipt.gas_used
    );
    println!(
        "MEASURE public_deposit_gas sell_token={} used={} limit={limit}",
        swap.terms.sell_token, receipt.gas_used
    );
    let deposit = deposit.clone();
    (receipt, deposit)
}

/// Read `swap`'s hand-off through the wallet once the block of `receipt`, its deposit's, is
/// final on the swap's chain. The wallet must record that transaction as the hand-off of
/// `deposit`, with the amounts its event carries.
async fn observe_public_handoff(
    wallet: &Wallet,
    fork: &ForkChain,
    swap: &PublicSwap,
    receipt: &TransactionReceipt,
    deposit: &SpokePool::FundsDeposited,
) {
    let origin = &wallet.chain;
    let owner = &wallet.destination.as_ref().unwrap().owner;
    let observe = || {
        owner.observe_public_swap_handoff(
            swap.operation,
            swap.id,
            origin,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };
    assert_eq!(
        observe().await.unwrap(),
        None,
        "the deposit's block isn't final yet"
    );
    fork.mine(origin.finality_depth).await;
    let handoff = observe()
        .await
        .unwrap()
        .expect("the final deposit is the swap's hand-off");
    assert_eq!(
        handoff,
        SwapBridgeHandoff {
            observation: SwapObservation {
                block: BlockNumHash::new(
                    receipt.block_number.unwrap(),
                    receipt.block_hash.unwrap()
                ),
                transaction_hash: Some(receipt.transaction_hash),
            },
            deposit_id: Some(deposit.depositId),
        }
    );
    let observed = public_swap_record(wallet, swap).0.observations();
    assert_eq!(observed.bridge_handoff, Some(handoff));
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: deposit.inputAmount,
            output_amount: deposit.outputAmount,
        })
    );
}

// A Public account on Ethereum deposits USDC into Across itself, for the handler on Polygon and
// the message that runs its new destination stealth account's shield. The relayer's fill
// shields the whole output to the wallet there, and the wallet verifies it from the fill's
// block.
//
// The record of a delivered swap takes no refund, so the withheld fill is a second deposit of
// the same account: Across reports it expired, refunds it on Ethereum, and the wallet verifies
// the refund to the Public account. Nothing is recovered: the tokens are back in the account.
//
// The account then sells ETH. That deposit takes no approval and pays its amount as the
// transaction's value, which the pool wraps.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_public_deposit_is_filled_and_shielded_on_the_destination_chain() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let wallet = Wallet::open(&fork).with_destination(&destination_fork);
    let origin = &wallet.chain;
    let destination = wallet.destination.as_ref().unwrap();
    let owner = &destination.owner;
    let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
    let profile = destination.chain.bridge_profile().unwrap();
    let (destination_spoke_pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
    let signer = public_signer("deposit");
    let source = signer.address();
    let (sold, minimum) = (
        U256::from(PUBLIC_DEPOSIT),
        U256::from(PUBLIC_DEPOSIT_MINIMUM),
    );
    let terms = PublicTerms {
        sell_token: USDC,
        sell_amount: sold,
        bridged_token: USDC,
        bridged_amount: sold,
        destination_token: DESTINATION_TOKEN,
        destination_minimum: minimum,
        hook_gas_limit: None,
    };
    let allowance = || ForkAllowance::allowanceCall {
        owner: source,
        spender: spoke_pool,
    };
    let approve = async |swap: &PublicSwap| {
        owner
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
    };

    // The account holds gas money and the USDC of two deposits.
    fork.impersonate(source).await;
    fork.add_erc20(USDC, source, sold * U256::from(2)).await;
    fund_relayer(&destination_fork, destination_spoke_pool).await;

    // The first deposit: one approval of the pool, the delivery signed against a quote dated
    // on the fork's clock, then the deposit itself.
    let swap = wallet.public_swap(&destination_fork, &signer, terms).await;
    assert_eq!(swap.plan.approval_gas_limits.len(), 1);
    approve(&swap).await.unwrap();
    assert_eq!(fork.call(USDC, allowance()).await, sold);
    let signed = wallet
        .sign_public_delivery(
            &swap,
            fork.timestamp().await,
            valid_until(PUBLIC_VALID_SECS),
        )
        .await;
    let held = fork.erc20_balance(USDC, source).await;
    let (receipt, deposit) =
        deposit_from_public_account(&wallet, &fork, &signer, &swap, &signed).await;
    assert_eq!(fork.erc20_balance(USDC, source).await, held - sold);
    observe_public_handoff(&wallet, &fork, &swap, &receipt, &deposit).await;

    // The fill pays the handler, which passes the output to the destination account, and that
    // account's shield puts all of it into Railgun.
    let filled = fill_and_shield(
        &destination_fork,
        destination,
        DESTINATION_TOKEN,
        &deposit,
        swap.account,
    )
    .await;
    verify_public_delivery(
        &wallet,
        &destination_fork,
        &swap,
        deposit.outputAmount,
        &filled,
    )
    .await;

    // The second deposit's fill is withheld, and Across reports it expired.
    let withheld = wallet.public_swap(&destination_fork, &signer, terms).await;
    approve(&withheld).await.unwrap();
    let signed = wallet
        .sign_public_delivery(
            &withheld,
            fork.timestamp().await,
            valid_until(PUBLIC_VALID_SECS),
        )
        .await;
    let held = fork.erc20_balance(USDC, source).await;
    let (receipt, deposit) =
        deposit_from_public_account(&wallet, &fork, &signer, &withheld, &signed).await;
    assert_eq!(fork.erc20_balance(USDC, source).await, held - sold);
    observe_public_handoff(&wallet, &fork, &withheld, &receipt, &deposit).await;
    let expired = |refund| {
        across_deposit_record(
            "expired",
            None,
            refund,
            handler,
            DESTINATION_CHAIN,
            deposit.outputAmount,
        )
    };
    let (across, reply, stub) = across_deposit_stub(expired(None)).await;
    assert_eq!(
        owner
            .check_public_swap_bridge(
                withheld.operation,
                withheld.id,
                &across,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        Some(SwapBridgeOutcome::Refunding)
    );
    let (refunding, shield) = public_swap_record(&wallet, &withheld);
    assert_eq!(shield, Some(SwapDestinationOutcome::Unfilled));
    assert_eq!(refunding.observations().bridge_refund, None);
    let refund = || {
        owner.observe_public_swap_refund(
            withheld.operation,
            withheld.id,
            origin,
            &across,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };
    // Across names no refund transaction yet.
    assert_eq!(refund().await.unwrap(), None);

    // The SpokePool refunds the deposit's input to the depositor, the Public account.
    fork.impersonate(spoke_pool).await;
    let refunded = fork
        .send(
            TransactionRequest::default()
                .from(spoke_pool)
                .to(USDC)
                .input(
                    ForkTransfer::transferCall {
                        to: source,
                        amount: deposit.inputAmount,
                    }
                    .abi_encode()
                    .into(),
                )
                .gas_limit(200_000),
        )
        .await;
    assert!(refunded.status(), "the SpokePool refunds the deposit");
    *reply.lock().unwrap() = expired(Some(refunded.transaction_hash));
    assert_eq!(
        refund().await.unwrap(),
        None,
        "the refund's block isn't final yet"
    );
    fork.mine(origin.finality_depth).await;
    let verified = Some(SwapObservation {
        block: BlockNumHash::new(refunded.block_number.unwrap(), refunded.block_hash.unwrap()),
        transaction_hash: Some(refunded.transaction_hash),
    });
    assert_eq!(refund().await.unwrap(), verified);
    let (saved, shield) = public_swap_record(&wallet, &withheld);
    assert_eq!(saved.observations().bridge_refund, verified);
    assert_eq!(
        saved.observations().bridge_outcome,
        Some(SwapBridgeOutcome::Refunding)
    );
    assert_eq!(shield, Some(SwapDestinationOutcome::Unfilled));
    stub.abort();
    // The tokens are back in the Public account. The destination account received nothing,
    // and its shield never ran.
    assert_eq!(fork.erc20_balance(USDC, source).await, held);
    let account = withheld.account.executor();
    assert_eq!(
        (
            destination_fork
                .erc20_balance(DESTINATION_TOKEN, account)
                .await,
            execution_nonce(&destination_fork, account).await
        ),
        (U256::ZERO, withheld.account.observed().nonce())
    );

    // ETH is deposited as WETH: no approval, and the amount as the transaction's value.
    let amount = U256::from(SELL_AMOUNT);
    let native = wallet
        .public_swap(
            &destination_fork,
            &signer,
            PublicTerms {
                sell_token: Address::ZERO,
                sell_amount: amount,
                bridged_token: WETH,
                bridged_amount: amount,
                destination_token: POLYGON_WETH,
                destination_minimum: amount - amount / U256::from(100),
                hook_gas_limit: None,
            },
        )
        .await;
    assert_eq!(native.plan.approval_gas_limits, [] as [u64; 0]);
    approve(&native).await.unwrap();
    assert_eq!(
        public_swap_record(&wallet, &native).0.transactions(),
        [] as [PublicSwapTransaction; 0]
    );
    let signed = wallet
        .sign_public_delivery(
            &native,
            fork.timestamp().await,
            valid_until(PUBLIC_VALID_SECS),
        )
        .await;
    let (ether, wrapped) = (
        fork.native_balance(source).await,
        fork.erc20_balance(WETH, source).await,
    );
    let (receipt, deposit) =
        deposit_from_public_account(&wallet, &fork, &signer, &native, &signed).await;
    let gas = U256::from(receipt.gas_used) * U256::from(receipt.effective_gas_price);
    assert_eq!(
        (
            fork.native_balance(source).await,
            fork.erc20_balance(WETH, source).await
        ),
        (ether - amount - gas, wrapped)
    );
    let sent = public_swap_record(&wallet, &native).0;
    assert!(matches!(
        sent.transactions(),
        [transaction] if transaction.kind == PublicSwapTransactionKind::Deposit
            && transaction.hash == receipt.transaction_hash
            && transaction.transaction.value == Some(amount)
    ));
    observe_public_handoff(&wallet, &fork, &native, &receipt, &deposit).await;
    wallet.finish().await;
}
