use super::*;

/// Settle a private Across order from fresh stealth accounts on both forks, and check its
/// deposit as [`placed_private_deposit`] does.
async fn private_deposit(
    wallet: &mut Wallet,
    fork: &ForkChain,
    destination_fork: &ForkChain,
    surplus: BridgeSurplus,
    on_shield_failure: BridgeShieldFailure,
) -> PrivateDeposit {
    let settled = wallet
        .settle_private_across(fork, destination_fork, surplus, on_shield_failure)
        .await;
    placed_private_deposit(wallet, settled, on_shield_failure)
}

/// The deposit of a settled private Across order, from fresh or reused stealth accounts. Its
/// post-hook must deposit, within its declared gas limit, for the destination chain's handler
/// with the message that drains the fill to the destination account and runs the shield that
/// account signed for the order's swap use, and the wallet must record the hand-off.
fn placed_private_deposit(
    wallet: &Wallet,
    (swap, receipt, terms): (Swap, TransactionReceipt, AcrossOrderTerms),
    on_shield_failure: BridgeShieldFailure,
) -> PrivateDeposit {
    let destination = wallet.destination.as_ref().unwrap();
    let account = wallet.destination_account.unwrap();
    let handler = destination
        .chain
        .bridge_profile()
        .unwrap()
        .multicall_handler();
    let destination_record = wallet.destination_record(account.operation());
    let Some(SwapUseRole::Destination {
        shields: signed, ..
    }) = destination_record
        .swap_use(swap.swap_use)
        .map(SwapUseRecord::role)
    else {
        panic!("the destination account serves the order's swap use");
    };
    let shields = destination_record
        .issued()
        .iter()
        .filter(|payload| {
            payload.purpose() == ExecutorPayloadPurpose::SwapDestinationShield
                && signed.contains(&payload.hash())
        })
        .map(|payload| payload.context().calldata().clone())
        .collect::<Vec<_>>();
    let [shield] = shields.as_slice() else {
        panic!("the destination account signs one shield for the swap use");
    };
    let fallback = match on_shield_failure {
        BridgeShieldFailure::RefundOnOrigin => None,
        BridgeShieldFailure::KeepOnDestination => Some(account.executor()),
    };
    let message = private_delivery_message(
        handler,
        destination.token,
        account.executor(),
        shield.clone(),
        fallback,
    );

    // The hooks trampoline drops a post-hook that runs out of gas, which leaves no deposit.
    let deposits = deposits(&receipt, terms.spoke_pool);
    let [deposit] = deposits.as_slice() else {
        panic!("the private post-hook deposits once within its gas limit");
    };
    assert_eq!(
        (deposit.depositor, deposit.recipient),
        (
            address_to_bytes32(swap.executor),
            address_to_bytes32(handler)
        )
    );
    assert_eq!(deposit.message, message);
    assert_eq!(
        (
            deposit.outputToken,
            deposit.outputAmount,
            deposit.destinationChainId
        ),
        (
            address_to_bytes32(destination.token),
            U256::from(DESTINATION_MINIMUM),
            U256::from(destination.chain.chain_id)
        )
    );
    assert_eq!(
        (terms.recipient, terms.message_hash),
        (Some(handler), Some(alloy::primitives::keccak256(&message)))
    );
    let record = wallet.record(swap.operation);
    assert_eq!(
        order_of(&record, swap.uid)
            .observations()
            .bridge_handoff
            .map(|handoff| (handoff.observation.transaction_hash, handoff.deposit_id)),
        Some((Some(receipt.transaction_hash), Some(deposit.depositId)))
    );
    PrivateDeposit {
        deposit: deposit.clone(),
        shield: shield.clone(),
        swap,
        receipt,
        terms,
        account,
    }
}

/// The wallet's own verification of `placed`'s delivery once `filled`'s block is final on the
/// destination fork: one poll of an Across stub whose deposit record names that block, then the
/// read of its receipts. Returns the outcome recorded with the swap and the one the destination
/// account's record takes from it.
async fn verify_fill(
    wallet: &Wallet,
    destination_fork: &ForkChain,
    placed: &PrivateDeposit,
    filled: &TransactionReceipt,
) -> (Option<SwapBridgeOutcome>, Option<SwapDestinationOutcome>) {
    let destination = wallet.destination.as_ref().unwrap();
    destination_fork
        .mine(destination.chain.finality_depth)
        .await;
    let located = json!({"deposit": {
        "status": "filled",
        "fillBlockNumber": filled.block_number.unwrap(),
        "fillTx": filled.transaction_hash,
        "outputAmount": DESTINATION_MINIMUM.to_string(),
        "recipient": placed.terms.recipient.unwrap(),
        "destinationChainId": destination.chain.chain_id.to_string()
    }})
    .to_string();
    let (url, _, stub) = spawn_bridge_stub(move |_| located.clone()).await;
    let http = || {
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        )
    };
    // Tracking asks only Across and the destination chain, never the orderbook.
    let orderbook =
        CowOrderbookClient::new(http(), "http://127.0.0.1:1/mainnet".parse().unwrap(), 1).unwrap();
    let mut clients = wallet.owner.swap_bridge_clients(&orderbook).unwrap();
    clients.across = crate::bridge::AcrossClient::new(http(), url).unwrap();
    let outcome = wallet
        .owner
        .observe_swap_bridge(
            placed.swap.operation,
            placed.swap.uid,
            &clients,
            Some(&destination.chain),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    stub.abort();
    destination.owner.reconcile_swap_destinations().unwrap();
    let settled = destination
        .owner
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == placed.account.operation())
        .unwrap()
        .swap_destination()
        .unwrap()
        .outcome;
    (outcome, settled)
}

/// What the wallet keeps of `placed`'s swap use: its order, with its terms, identifier,
/// observations and bridge outcome, and the use's record on the source and on the destination
/// account, which holds the destination outcome.
fn use_history(
    wallet: &Wallet,
    placed: &PrivateDeposit,
) -> (
    SwapOrderRecord,
    Option<SwapUseRecord>,
    Option<SwapUseRecord>,
) {
    let source = wallet.record(placed.swap.operation);
    let destination = wallet.destination_record(placed.account.operation());
    let swap_use = placed.swap.swap_use;
    (
        order_of(&source, placed.swap.uid).clone(),
        source.swap_use(swap_use).cloned(),
        destination.swap_use(swap_use).cloned(),
    )
}

/// The note that `filled`'s shield of `value` of the destination token created, as the wallet's
/// synced notes hold it once every active POI list accepted it.
fn accepted_shield_note(
    wallet: &Wallet,
    filled: &TransactionReceipt,
    value: U256,
) -> crate::WalletUtxo {
    let mut utxo = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            wallet.view.scan_keys().master_public_key,
            wallet.destination.as_ref().unwrap().token,
            value,
            [9; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: filled.transaction_hash,
            block_number: filled.block_number.unwrap(),
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    for list in poi::poi::default_active_poi_list_keys() {
        utxo.poi.statuses.insert(list, crate::PoiStatus::Valid);
    }
    crate::WalletUtxo::new(utxo)
}

// A relayer's fill of a private Across deposit pays the destination chain's handler, which
// passes the tokens to the destination stealth account and runs its pre-signed shield, all in
// the fill's transaction. Before the fill that shield reverts for anyone without using the
// account's nonce. Without a fallback recipient, a shield that fails reverts the whole fill.
//
// Once that swap is delivered and verified, both of its accounts serve a second private swap
// under a new use, without a setup on either chain. Each signs at its current nonce, the second
// fill shields to the wallet as the first did, the first swap's payloads can't run again, and
// what the wallet keeps of the first swap stays as it was.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_fill_shields_to_the_wallet_on_the_destination_chain() {
    Box::pin(private_across_fill_scenario(
        DESTINATION_CHAIN,
        DESTINATION_FORK_RPC_URL_ENV,
        DESTINATION_TOKEN,
    ))
    .await;
}

#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, BASE_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_fill_shields_to_the_wallet_on_base() {
    Box::pin(private_across_fill_scenario(
        BASE,
        BASE_FORK_RPC_URL_ENV,
        BASE_USDC,
    ))
    .await;
}

/// The fill and second use described above, with the private Across orders delivering `token`
/// to the chain `chain_id`, forked from the RPC that `rpc_env` names.
async fn private_across_fill_scenario(chain_id: u64, rpc_env: &str, token: Address) {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination_from(rpc_env, chain_id).await;
    let mut wallet = Wallet::open(&fork).with_destination_on(&destination_fork, chain_id, token);
    let placed = private_deposit(
        &mut wallet,
        &fork,
        &destination_fork,
        BridgeSurplus::KeepInAccount,
        BridgeShieldFailure::RefundOnOrigin,
    )
    .await;
    let destination = wallet.destination.as_ref().unwrap();
    let profile = destination.chain.bridge_profile().unwrap();
    let (spoke_pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
    let railgun = destination
        .chain
        .require_railgun()
        .unwrap()
        .deployment
        .contract;
    let account = placed.account.executor();
    let output = U256::from(DESTINATION_MINIMUM);
    let balance = |holder| destination_fork.erc20_balance(token, holder);
    let nonce = execution_nonce(&destination_fork, account).await;
    assert_eq!(nonce, placed.account.observed().nonce());
    assert!(
        destination_fork.timestamp().await < u64::from(placed.terms.fill_deadline),
        "the destination fork's clock is before the fill deadline"
    );
    fund_relayer_with(
        &destination_fork,
        spoke_pool,
        token,
        U256::from(10 * DESTINATION_MINIMUM),
    )
    .await;
    let funded = balance(RELAYER).await;
    let held_by_handler = balance(handler).await;

    // The message is public from the deposit onward. Someone runs the shield before the fill.
    let snapshot = destination_fork.snapshot().await;
    let early = destination_fork
        .send_as_bypass(account, placed.shield.clone(), FILL_GAS)
        .await;
    assert!(
        !early.status(),
        "the shield's guard reverts before the fill"
    );
    assert_eq!(execution_nonce(&destination_fork, account).await, nonce);
    destination_fork.revert(snapshot).await;

    // Another payload of the account took the shield's nonce, so the shield fails in the fill.
    // The message names no fallback recipient, so the fill reverts and the relayer keeps its
    // tokens.
    let snapshot = destination_fork.snapshot().await;
    destination_fork
        .set_storage(account, EXECUTION_NONCE_STORAGE_SLOT, nonce + U256::ONE)
        .await;
    let (failed, reverts) = fill_on(
        &destination_fork,
        spoke_pool,
        relay_of(&placed.deposit),
        chain_id,
    )
    .await;
    assert!(
        !failed.status() && reverts.is_some(),
        "a failed shield reverts a fill without a fallback recipient"
    );
    assert_eq!(
        (balance(RELAYER).await, balance(account).await),
        (funded, U256::ZERO)
    );
    destination_fork.revert(snapshot).await;

    // A plain fill of the same deposit to an address without code, on the same state.
    let snapshot = destination_fork.snapshot().await;
    let plain = ForkSpokePool::V3RelayData {
        recipient: address_to_bytes32(BRIDGE_RECEIVER),
        message: Bytes::new(),
        ..relay_of(&placed.deposit)
    };
    let (plain_fill, reverts) = fill_on(&destination_fork, spoke_pool, plain, chain_id).await;
    assert!(plain_fill.status(), "the plain fill pays: {reverts:?}");
    destination_fork.revert(snapshot).await;

    let shielded_before = balance(railgun).await;
    let (filled, reverts) = fill_on(
        &destination_fork,
        spoke_pool,
        relay_of(&placed.deposit),
        chain_id,
    )
    .await;
    assert!(
        filled.status(),
        "the fill runs the handler's instructions and the shield: {reverts:?}"
    );
    // The fill carries the signed message's hash, which the wallet matches it by.
    let fills = filled
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == spoke_pool)
        .filter_map(|log| log.log_decode::<SpokePool::FilledRelay>().ok())
        .map(|log| log.inner.data)
        .collect::<Vec<_>>();
    let [relayed] = fills.as_slice() else {
        panic!("the SpokePool fills once");
    };
    assert_eq!(
        (
            relayed.recipient,
            Some(relayed.messageHash),
            Some(relayed.relayExecutionInfo.updatedMessageHash)
        ),
        (
            address_to_bytes32(handler),
            placed.terms.message_hash,
            placed.terms.message_hash
        )
    );
    // The output moves from the relayer through the handler and the account into Railgun,
    // less the shield fee, which goes to Railgun's treasury.
    let shields = shields_of(&filled, railgun, token);
    let [(credited, fee)] = shields.as_slice() else {
        panic!("the fill shields once");
    };
    assert!(!fee.is_zero());
    assert_eq!(*credited + *fee, output);
    assert_eq!(balance(railgun).await, shielded_before + *credited);
    assert_eq!(
        (
            balance(RELAYER).await,
            balance(handler).await,
            balance(account).await
        ),
        (funded - output, held_by_handler, U256::ZERO)
    );
    assert_eq!(
        execution_nonce(&destination_fork, account).await,
        nonce + U256::ONE
    );

    let handler_gas = destination_fork
        .call_gas(
            filled.transaction_hash,
            handler,
            &MulticallHandler::handleV3AcrossMessageCall::SELECTOR,
        )
        .await;
    let shield_gas = destination_fork
        .call_gas(filled.transaction_hash, account, &placed.shield)
        .await;
    println!(
        "MEASURE destination_fill_gas handler_fill={} plain_fill={}",
        filled.gas_used, plain_fill.gas_used
    );
    println!(
        "MEASURE destination_fill_call_gas handler_message={handler_gas:?} account_shield={shield_gas:?}"
    );

    // The wallet verifies the delivery from the fill block's receipts, and the destination
    // account's record takes its payload's outcome from the swap.
    let block = BlockNumHash::new(filled.block_number.unwrap(), filled.block_hash.unwrap());
    let transaction_hash = filled.transaction_hash;
    assert_eq!(
        verify_fill(&wallet, &destination_fork, &placed, &filled).await,
        (
            Some(SwapBridgeOutcome::DeliveredVerified {
                block,
                transaction_hash,
                output_amount: output,
                shielded: true,
            }),
            Some(SwapDestinationOutcome::Shielded {
                block,
                transaction_hash,
            })
        )
    );

    // A second swap reuses both accounts. The fork wallet has no synced session, so the POI
    // verdict of the first shield's note is stubbed here: the note is stated as accepted on
    // every active list. The delegations, nonces, balances, settlement, fill and shield are
    // the forks' own.
    let first = use_history(&wallet, &placed);
    let source = placed.swap.executor;
    let source_nonce = execution_nonce(&fork, source).await;
    let notes = AcceptedShieldNotes(vec![accepted_shield_note(&wallet, &filled, *credited)]);
    let reused = wallet
        .reuse_private(&fork, &destination_fork, &placed, notes)
        .await;
    assert_eq!(use_history(&wallet, &placed), first);
    let swap = wallet.swap_from(&fork, reused).await;
    let settled = wallet.settle_deposit(&fork, swap).await;
    let again = placed_private_deposit(&wallet, settled, BridgeShieldFailure::RefundOnOrigin);
    let second_use = again.swap.swap_use;
    assert_ne!(second_use, placed.swap.swap_use);

    // Each account signed at its current nonce, past every nonce of the first swap, and
    // neither signed another setup.
    let source_record = wallet.record(again.swap.operation);
    assert_eq!(
        order_of(&source_record, again.swap.uid).use_id(),
        Some(second_use)
    );
    let setup_nonce = source_record.issued()[0].nonce();
    assert_eq!(source_nonce, setup_nonce + U256::from(3));
    assert_eq!(
        issued_nonces(&source_record),
        [
            (ExecutorPayloadPurpose::Operation, setup_nonce),
            (ExecutorPayloadPurpose::SwapPreHook, setup_nonce + U256::ONE),
            (
                ExecutorPayloadPurpose::SwapPostHook,
                setup_nonce + U256::from(2)
            ),
            (ExecutorPayloadPurpose::SwapPreHook, source_nonce),
            (
                ExecutorPayloadPurpose::SwapPostHook,
                source_nonce + U256::ONE
            ),
        ]
    );
    assert_eq!(
        execution_nonce(&fork, source).await,
        source_nonce + U256::from(2)
    );
    assert_eq!(
        issued_nonces(&wallet.destination_record(again.account.operation())),
        [
            (ExecutorPayloadPurpose::Operation, nonce - U256::ONE),
            (ExecutorPayloadPurpose::SwapDestinationShield, nonce),
            (
                ExecutorPayloadPurpose::SwapDestinationShield,
                nonce + U256::ONE
            ),
        ]
    );
    assert_eq!(again.account.observed().nonce(), nonce + U256::ONE);
    assert_eq!(
        execution_nonce(&destination_fork, account).await,
        nonce + U256::ONE
    );

    // The second fill moves its output through the handler and the same account into Railgun,
    // less the shield fee, and uses the account's next nonce.
    assert!(
        destination_fork.timestamp().await < u64::from(again.terms.fill_deadline),
        "the destination fork's clock is before the second fill deadline"
    );
    let (relayer_before, shielded_before) = (balance(RELAYER).await, balance(railgun).await);
    let (refilled, reverts) = fill_on(
        &destination_fork,
        spoke_pool,
        relay_of(&again.deposit),
        chain_id,
    )
    .await;
    assert!(
        refilled.status(),
        "the second fill runs the reused account's shield: {reverts:?}"
    );
    let reshields = shields_of(&refilled, railgun, token);
    let [(recredited, refee)] = reshields.as_slice() else {
        panic!("the second fill shields once");
    };
    assert!(!refee.is_zero());
    assert_eq!(*recredited + *refee, output);
    assert_eq!(balance(railgun).await, shielded_before + *recredited);
    assert_eq!(
        (
            balance(RELAYER).await,
            balance(handler).await,
            balance(account).await
        ),
        (relayer_before - output, held_by_handler, U256::ZERO)
    );
    assert_eq!(
        execution_nonce(&destination_fork, account).await,
        nonce + U256::from(2)
    );

    // The wallet verifies the second delivery and records it under the second use. The first
    // use keeps its own outcome, and the first swap's order and uses are what they were.
    let reblock = BlockNumHash::new(refilled.block_number.unwrap(), refilled.block_hash.unwrap());
    let redelivered = SwapDestinationOutcome::Shielded {
        block: reblock,
        transaction_hash: refilled.transaction_hash,
    };
    assert_eq!(
        verify_fill(&wallet, &destination_fork, &again, &refilled).await,
        (
            Some(SwapBridgeOutcome::DeliveredVerified {
                block: reblock,
                transaction_hash: refilled.transaction_hash,
                output_amount: output,
                shielded: true,
            }),
            Some(redelivered)
        )
    );
    let destination_record = wallet.destination_record(again.account.operation());
    let outcome_of = |swap_use| {
        destination_record
            .swap_destination_use(swap_use)
            .and_then(|served| served.outcome)
    };
    assert_eq!(
        (outcome_of(placed.swap.swap_use), outcome_of(second_use)),
        (
            Some(SwapDestinationOutcome::Shielded {
                block,
                transaction_hash,
            }),
            Some(redelivered)
        )
    );
    assert_eq!(use_history(&wallet, &placed), first);

    // The first swap's payloads stay public and signed, but their nonces are consumed. Its
    // hooks, replayed on the swap's chain, revert and move nothing.
    let held = (
        fork.erc20_balance(WETH, source).await,
        fork.erc20_balance(USDC, source).await,
    );
    for hook in placed.swap.hooks.pre.iter().chain(&placed.swap.hooks.post) {
        let replayed = fork
            .send_as_bypass(hook.target, hook.call_data.clone(), 3_000_000)
            .await;
        assert!(!replayed.status(), "a hook at a consumed nonce reverts");
    }
    assert_eq!(
        execution_nonce(&fork, source).await,
        source_nonce + U256::from(2)
    );
    assert_eq!(
        (
            fork.erc20_balance(WETH, source).await,
            fork.erc20_balance(USDC, source).await
        ),
        held
    );
    // Its shield, replayed on the destination chain while the account holds enough to pass
    // the guard, reverts on its nonce alone and leaves the tokens in the account.
    destination_fork.add_erc20(token, account, output).await;
    let shielded = balance(railgun).await;
    let replayed = destination_fork
        .send_as_bypass(account, placed.shield.clone(), FILL_GAS)
        .await;
    assert!(!replayed.status(), "a shield at a consumed nonce reverts");
    assert_eq!(
        (
            execution_nonce(&destination_fork, account).await,
            balance(account).await,
            balance(railgun).await
        ),
        (nonce + U256::from(2), output, shielded)
    );
    wallet.finish().await;
}

// With the destination stealth account as fallback recipient, a fill whose shield fails still
// completes: the handler reverts its instructions and passes the tokens to the account, which
// holds them for recovery there.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_fill_with_a_failed_shield_is_held_by_the_destination_account() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let mut wallet = Wallet::open(&fork).with_destination(&destination_fork);
    let placed = private_deposit(
        &mut wallet,
        &fork,
        &destination_fork,
        BridgeSurplus::KeepInAccount,
        BridgeShieldFailure::KeepOnDestination,
    )
    .await;
    let destination = wallet.destination.as_ref().unwrap();
    let spoke_pool = destination.chain.bridge_profile().unwrap().spoke_pool();
    let railgun = destination
        .chain
        .require_railgun()
        .unwrap()
        .deployment
        .contract;
    let account = placed.account.executor();
    let output = U256::from(DESTINATION_MINIMUM);
    let balance = |holder| destination_fork.erc20_balance(DESTINATION_TOKEN, holder);
    fund_relayer(&destination_fork, spoke_pool).await;

    // Another payload of the account took the shield's nonce, so the shield fails in the fill.
    let taken = execution_nonce(&destination_fork, account).await + U256::ONE;
    destination_fork
        .set_storage(account, EXECUTION_NONCE_STORAGE_SLOT, taken)
        .await;
    let shielded_before = balance(railgun).await;
    let (filled, reverts) = fill(&destination_fork, spoke_pool, relay_of(&placed.deposit)).await;
    assert!(
        filled.status(),
        "the handler passes a failed shield's tokens to the fallback recipient: {reverts:?}"
    );
    assert_eq!(
        (balance(account).await, balance(railgun).await),
        (output, shielded_before)
    );
    assert_eq!(
        shields_of(&filled, railgun, DESTINATION_TOKEN),
        [] as [(U256, U256); 0]
    );
    assert_eq!(execution_nonce(&destination_fork, account).await, taken);

    let block = BlockNumHash::new(filled.block_number.unwrap(), filled.block_hash.unwrap());
    let transaction_hash = filled.transaction_hash;
    assert_eq!(
        verify_fill(&wallet, &destination_fork, &placed, &filled).await,
        (
            Some(SwapBridgeOutcome::HeldOnDestination {
                block,
                transaction_hash,
                amount: output,
            }),
            Some(SwapDestinationOutcome::Held {
                block,
                transaction_hash,
            })
        )
    );
    wallet.finish().await;
}

/// Intrinsic gas of `data` as transaction calldata.
fn calldata_gas(data: &[u8]) -> u64 {
    data.iter()
        .map(|byte| if *byte == 0 { 4 } else { 16 })
        .sum()
}

// The handler message makes a private Across post-hook larger and its `FundsDeposited` event
// longer than a plain one's. The post-hook still deposits within the gas limit its plan declares,
// for either surplus choice.
#[tokio::test]
#[ignore = "needs ETH_FORK_RPC_URL, DESTINATION_FORK_RPC_URL and anvil"]
async fn swap_fork_private_across_post_hook_deposits_within_its_gas_limit() {
    let fork = ForkChain::start().await;
    let destination_fork = ForkChain::start_destination(DESTINATION_CHAIN).await;
    let mut wallet = Wallet::open(&fork).with_destination(&destination_fork);
    for (surplus, label) in SURPLUS_CHOICES {
        let (plain, plain_receipt, terms) = wallet.settle_across(&fork, surplus).await;
        assert_eq!(deposits(&plain_receipt, terms.spoke_pool).len(), 1);
        let placed = private_deposit(
            &mut wallet,
            &fork,
            &destination_fork,
            surplus,
            BridgeShieldFailure::RefundOnOrigin,
        )
        .await;
        let (plain_hook, private_hook) = (&plain.hooks.post[0], &placed.swap.hooks.post[0]);
        let plain_gas = fork
            .call_gas(
                plain_receipt.transaction_hash,
                plain.executor,
                &plain_hook.call_data,
            )
            .await;
        let private_gas = fork
            .call_gas(
                placed.receipt.transaction_hash,
                placed.swap.executor,
                &private_hook.call_data,
            )
            .await;
        println!(
            "MEASURE across_post_hook_gas surplus={label} plain={plain_gas:?} private={private_gas:?}"
        );
        println!(
            "MEASURE across_post_hook_gas_limit surplus={label} plain={} private={}",
            plain_hook.gas_limit, private_hook.gas_limit
        );
        println!(
            "MEASURE across_post_hook_calldata_gas surplus={label} plain={} private={}",
            calldata_gas(&plain_hook.call_data),
            calldata_gas(&private_hook.call_data)
        );
        println!(
            "MEASURE across_settlement_gas surplus={label} plain={} private={}",
            plain_receipt.gas_used, placed.receipt.gas_used
        );
    }
    wallet.finish().await;
}
