use super::*;

// An ERC-20 deposit from an account without an allowance sends the exact approval, then the
// deposit. The path and its terms are in the record before the deposit is broadcast, each
// transaction is in it before its own broadcast, and the hand-off is read from the deposit's
// block once that block is final.
#[tokio::test(start_paused = true)]
async fn a_public_swap_approves_then_deposits_with_its_path_recorded_first() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::ONE).await;

    // A short allowance that isn't zero takes a reset and an approval. The review covered one
    // approval, so nothing is sent and the swap returns to review with its approval as saved.
    let approval = payment.saved().swap.approval().clone();
    assert_eq!(
        payment.approve(&owner).await.unwrap(),
        PublicSwapApprovalsOutcome::ReviewRequired { transactions: true }
    );
    assert_eq!(
        payment.chain.lock().unwrap().broadcasts,
        [] as [(B256, TxEnvelope); 0]
    );
    assert_eq!(
        payment.saved().swap.transactions(),
        [] as [PublicSwapTransaction; 0]
    );
    assert_eq!(payment.saved().swap.approval(), &approval);

    payment.chain.lock().unwrap().allowance = U256::ZERO;
    payment.approve(&owner).await.unwrap();
    let PublicSwapTransactionOutcome::Included {
        block_number,
        transaction_hash,
    } = payment.deposit(&owner).await.unwrap()
    else {
        panic!("the deposit succeeds");
    };
    assert_eq!(block_number, ORIGIN_HEAD + 2);

    // The approval is the exact one to the pinned pool, and the deposit carries no value.
    let broadcasts = payment.chain.lock().unwrap().broadcasts.clone();
    let [(approval_hash, approval), (deposit_hash, deposit)] = broadcasts.as_slice() else {
        panic!("an approval and a deposit were broadcast");
    };
    assert_eq!(
        (approval.to(), approval.nonce(), &approval.input()[..]),
        (
            Some(DESTINATION_TOKEN),
            0,
            build_approve_calldata(payment.spoke_pool, payment.sell_amount).as_slice()
        )
    );
    assert_eq!(
        (deposit.to(), deposit.nonce(), deposit.value()),
        (Some(payment.spoke_pool), 1, U256::ZERO)
    );
    assert_eq!(*deposit_hash, transaction_hash);
    assert_eq!(
        payment.chain.lock().unwrap().allowance_reads,
        [(DESTINATION_TOKEN, payment.source, payment.spoke_pool); 2]
    );

    // The record holds the path, the terms and both transactions with their inclusions.
    let recorded = payment.saved().swap;
    assert_eq!(recorded.path(), Some(&PublicSwapPath::Deposit));
    assert_eq!(recorded.bridge(), Some(&payment.terms));
    let [approved, deposited] = recorded.transactions() else {
        panic!("both transactions are recorded");
    };
    for (transaction, kind, hash, head) in [
        (
            approved,
            PublicSwapTransactionKind::Approval,
            approval_hash,
            ORIGIN_HEAD,
        ),
        (
            deposited,
            PublicSwapTransactionKind::Deposit,
            deposit_hash,
            ORIGIN_HEAD + 1,
        ),
    ] {
        assert_eq!((transaction.kind, transaction.hash), (kind, *hash));
        assert_eq!(transaction.submitted_from_block, Some(head));
        let inclusion = transaction.inclusion.unwrap();
        assert!(inclusion.succeeded);
        assert_eq!(
            (
                inclusion.observation.block.number,
                inclusion.observation.transaction_hash
            ),
            (head + 1, Some(*hash))
        );
    }

    // Each broadcast found its own transaction recorded and not yet included, and the
    // deposit's found the path and the terms.
    {
        let at_broadcast = payment.at_broadcast.lock().unwrap();
        let [(_, at_approval), (_, at_deposit)] = at_broadcast.as_slice() else {
            panic!("two broadcasts arrived");
        };
        assert_eq!(at_approval.path(), None);
        assert!(matches!(
            at_approval.transactions(),
            [transaction] if transaction.hash == *approval_hash && transaction.inclusion.is_none()
        ));
        assert_eq!(at_deposit.path(), Some(&PublicSwapPath::Deposit));
        assert_eq!(at_deposit.bridge(), Some(&payment.terms));
        assert!(matches!(
            at_deposit.transactions(),
            [_, transaction] if transaction.hash == *deposit_hash && transaction.inclusion.is_none()
        ));
    }

    // The hand-off waits for the deposit's block to be final, and is then read from that
    // block's receipts without a log query.
    let observe = || {
        owner.observe_public_swap_handoff(
            payment.operation,
            payment.id,
            &payment.origin,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };
    assert_eq!(observe().await.unwrap(), None);
    payment.mine();
    let handoff = observe().await.unwrap().unwrap();
    assert_eq!(handoff.deposit_id, Some(U256::from(DEPOSIT_ID)));
    assert_eq!(
        (
            handoff.observation.block.number,
            handoff.observation.transaction_hash
        ),
        (block_number, Some(transaction_hash))
    );
    let observations = payment.saved().swap.observations();
    assert_eq!(observations.bridge_handoff, Some(handoff));
    assert_eq!(
        observations.deposited,
        Some(PublicSwapDeposited {
            input_amount: payment.sell_amount,
            output_amount: U256::from(1_000),
        })
    );
    assert!(
        !payment
            .chain
            .lock()
            .unwrap()
            .methods()
            .any(|method| method == "eth_getLogs")
    );
    payment.finish(owner).await;
}

// The native asset is deposited as the transaction's value: one transaction, no approval and
// no allowance read.
#[tokio::test(start_paused = true)]
async fn a_native_public_swap_deposits_its_amount_as_value_without_an_approval() {
    let (payment, owner) = PublicPayment::start(Address::ZERO, U256::ZERO).await;
    payment.approve(&owner).await.unwrap();
    assert_eq!(
        payment.chain.lock().unwrap().broadcasts,
        [] as [(B256, TxEnvelope); 0]
    );
    assert!(matches!(
        payment.deposit(&owner).await.unwrap(),
        PublicSwapTransactionOutcome::Included { .. }
    ));

    let broadcasts = payment.chain.lock().unwrap().broadcasts.clone();
    let [(hash, deposit)] = broadcasts.as_slice() else {
        panic!("only the deposit was broadcast");
    };
    assert_eq!(
        (deposit.to(), deposit.value()),
        (Some(payment.spoke_pool), payment.sell_amount)
    );
    assert_eq!(
        payment.chain.lock().unwrap().allowance_reads,
        [] as [(Address, Address, Address); 0]
    );
    let recorded = payment.saved().swap;
    assert!(matches!(
        recorded.transactions(),
        [transaction] if transaction.kind == PublicSwapTransactionKind::Deposit
            && transaction.hash == *hash
    ));
    // A repeat call must not issue a fresh nonce and spend the native amount again.
    assert!(payment.deposit(&owner).await.is_err());
    assert_eq!(payment.chain.lock().unwrap().broadcasts.len(), 1);
    payment.finish(owner).await;
}

// A deposit that reverts bridged nothing: its failed inclusion is recorded, no hand-off ever
// is, and the destination account's shield stays recorded as it was.
#[tokio::test(start_paused = true)]
async fn a_reverted_public_swap_deposit_hands_nothing_off() {
    for succeeds_after_reorg in [false, true] {
        let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
        payment.chain.lock().unwrap().deposits_revert = true;
        let before = payment.saved();
        let shield_state = before.record.payload_state(payment.shield);
        assert_eq!(before.shields, [payment.shield]);

        let PublicSwapTransactionOutcome::Reverted {
            block_number,
            transaction_hash,
        } = payment.deposit(&owner).await.unwrap()
        else {
            panic!("the deposit reverts");
        };
        let after = payment.saved();
        let [deposit] = after.swap.transactions() else {
            panic!("only the deposit was sent");
        };
        let inclusion = deposit.inclusion.unwrap();
        assert_eq!(
            (
                deposit.hash,
                inclusion.succeeded,
                inclusion.observation.block.number
            ),
            (transaction_hash, false, block_number)
        );

        assert!(!inclusion.finalized);
        assert!(!after.swap.is_finished(false, unix_now()));
        assert_eq!(after.record.public_swaps_to_track().count(), 1);
        if succeeds_after_reorg {
            let event = payment.deposited(payment.terms.input_amount, payment.terms.output_amount);
            let mut origin = payment.chain.lock().unwrap();
            let transaction = origin.broadcasts[0].1.clone();
            origin.chain.reorg(block_number);
            origin.chain.head = block_number + 2;
            origin.chain.include(
                block_number + 2,
                transaction,
                payment.source,
                true,
                vec![(payment.spoke_pool, event.encode_log_data())],
            );
        }
        // Final observation may verify failure, or discover success after provisional failure.
        payment.mine();
        let handoff = owner
            .observe_public_swap_handoff(
                payment.operation,
                payment.id,
                &payment.origin,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(handoff.is_some(), succeeds_after_reorg);
        let observed = payment.saved();
        assert_eq!(observed.swap.observations().bridge_handoff, handoff);
        let finalized = observed.swap.transactions()[0].inclusion.unwrap();
        assert!(finalized.finalized);
        assert_eq!(finalized.succeeded, succeeds_after_reorg);
        assert_eq!(
            observed.swap.is_finished(false, unix_now()),
            !succeeds_after_reorg
        );
        if succeeds_after_reorg {
            assert_eq!(finalized.observation.block.number, block_number + 2);
            assert_eq!(
                observed.swap.observations().deposited,
                Some(PublicSwapDeposited {
                    input_amount: payment.terms.input_amount,
                    output_amount: payment.terms.output_amount,
                })
            );
        }
        assert_eq!(observed.shields, [payment.shield]);
        assert_eq!(observed.record.payload_state(payment.shield), shield_state);
        assert!(
            !payment
                .chain
                .lock()
                .unwrap()
                .methods()
                .any(|method| method == "eth_getLogs")
        );
        payment.finish(owner).await;
    }
}

// The wallet stops between the deposit's broadcast and its receipt. The record already holds
// the transaction, and a new owner finds the deposit by matching its hash locally in bounded
// finalized block scans without sending the hash to RPC.
#[tokio::test]
async fn a_restart_after_the_deposit_broadcast_finds_its_handoff_in_finalized_blocks() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    tokio::select! {
        outcome = payment.deposit(&owner) => {
            panic!("the wallet stops before the receipt, but the deposit returned {outcome:?}");
        }
        () = payment.broadcast.notified() => {}
    }
    owner.shutdown().await;
    drop(owner);
    let interrupted = payment.saved().swap;
    let [deposit] = interrupted.transactions() else {
        panic!("the deposit was handed off");
    };
    assert_eq!(
        (
            deposit.kind,
            deposit.inclusion,
            deposit.submitted_from_block
        ),
        (PublicSwapTransactionKind::Deposit, None, Some(ORIGIN_HEAD))
    );
    let hash = deposit.hash;

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let observe = || {
        restarted.observe_public_swap_handoff(
            payment.operation,
            payment.id,
            &payment.origin,
            crate::OperationHttpClient::direct_for_tests(),
        )
    };
    // The deposit's block isn't final yet.
    assert_eq!(observe().await.unwrap(), None);
    payment.mine();
    let handoff = observe().await.unwrap().unwrap();
    assert_eq!(handoff.deposit_id, Some(U256::from(DEPOSIT_ID)));
    assert_eq!(
        (
            handoff.observation.block.number,
            handoff.observation.transaction_hash
        ),
        (ORIGIN_HEAD + 1, Some(hash))
    );
    let found = payment.saved().swap;
    assert_eq!(found.observations().bridge_handoff, Some(handoff));
    let inclusion = found.transactions()[0].inclusion.unwrap();
    assert!(inclusion.succeeded);
    assert_eq!(inclusion.observation, handoff.observation);

    // Discovery sends block identifiers only; no transaction-hash request reveals the deposit.
    {
        let chain = payment.chain.lock().unwrap();
        assert!(!chain.methods().any(|method| matches!(
            method,
            "eth_getLogs" | "eth_getTransactionReceipt" | "eth_getTransactionByHash"
        )));
        for request in &chain.requests {
            assert!(
                !request["params"]
                    .to_string()
                    .contains(&alloy::hex::encode(hash))
            );
        }
    }
    // A hand-off that is recorded is returned without another read.
    let served = payment.chain.lock().unwrap().requests.len();
    assert_eq!(observe().await.unwrap(), Some(handoff));
    assert_eq!(payment.chain.lock().unwrap().requests.len(), served);
    payment.finish(restarted).await;
}

// An interrupted failed deposit has no event to locate it. Final-block scan progress survives
// another restart, and a failed whole-block read cannot skip its unreconciled block.
#[tokio::test]
async fn an_interrupted_failed_deposit_resumes_bounded_final_block_scans() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    tokio::select! {
        outcome = payment.deposit(&owner) => panic!("deposit returned before interruption: {outcome:?}"),
        () = payment.broadcast.notified() => {}
    }
    owner.shutdown().await;
    drop(owner);
    let failed_block = ORIGIN_HEAD + 12;
    let (hash, transaction) = payment.chain.lock().unwrap().broadcasts[0].clone();
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.chain.reorg(ORIGIN_HEAD + 1);
        origin
            .chain
            .include(failed_block, transaction, payment.source, false, Vec::new());
        origin.chain.head = failed_block + 1;
        origin.requests.clear();
    }
    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    assert!(payment.deposit(&restarted).await.is_err());
    assert_eq!(payment.chain.lock().unwrap().broadcasts.len(), 1);
    assert_eq!(
        restarted
            .observe_public_swap_handoff(
                payment.operation,
                payment.id,
                &payment.origin,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        None
    );
    // Eight blocks from the finality depth below the head the deposit was sent at.
    let paged = payment.saved();
    let deposit = &paged.swap.transactions()[0];
    assert_eq!(deposit.deposit_scan_from_block, Some(ORIGIN_HEAD + 7));
    assert_eq!(deposit.inclusion, None);
    assert_eq!(paged.record.public_swaps_to_track().count(), 1);
    restarted.shutdown().await;
    drop(restarted);
    let restarted = ExecutorOwner::new(
        2,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    payment.chain.lock().unwrap().chain.receipt_error = Some(-32601);
    assert!(
        restarted
            .observe_public_swap_handoff(
                payment.operation,
                payment.id,
                &payment.origin,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    assert_eq!(
        payment.saved().swap.transactions()[0].deposit_scan_from_block,
        Some(ORIGIN_HEAD + 7)
    );
    payment.chain.lock().unwrap().chain.receipt_error = None;
    assert_eq!(
        restarted
            .observe_public_swap_handoff(
                payment.operation,
                payment.id,
                &payment.origin,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        None
    );
    let failed = payment.saved();
    let inclusion = failed.swap.transactions()[0].inclusion.unwrap();
    assert_eq!(
        (
            inclusion.observation.block.number,
            inclusion.succeeded,
            inclusion.finalized
        ),
        (failed_block, false, true)
    );
    assert!(failed.swap.is_finished(false, unix_now()));
    assert_eq!(failed.record.public_swaps_to_track().count(), 0);
    assert_eq!(failed.shields, [payment.shield]);
    {
        let origin = payment.chain.lock().unwrap();
        assert!(!origin.methods().any(|method| matches!(
            method,
            "eth_getTransactionReceipt" | "eth_getTransactionByHash" | "eth_getLogs"
        )));
        for request in &origin.requests {
            assert!(
                !request["params"]
                    .to_string()
                    .contains(&alloy::hex::encode(hash))
            );
        }
    }
    payment.finish(restarted).await;
}

// No RPC takes the deposit's send. The deposit still returns, handed off with the bytes it
// was signed as, rather than waiting for a receipt. After a restart, tracking resends exactly
// those bytes, signing nothing and recording no other deposit, and a refused resend fails
// nothing. A swap that was stopped resends nothing, though its deposit is still read.
#[tokio::test]
async fn a_deposit_no_rpc_took_is_resent_unchanged_after_a_restart() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, _reply, _lookups, across_task) = across_stub().await;
    payment.chain.lock().unwrap().rejects_sends = true;
    let PublicSwapTransactionOutcome::HandedOff { transaction_hash } =
        payment.deposit(&owner).await.unwrap()
    else {
        panic!("the deposit is handed off");
    };
    let signed = {
        let chain = payment.chain.lock().unwrap();
        assert_eq!(chain.broadcasts, [] as [(B256, TxEnvelope); 0]);
        let [signed] = chain.sent.as_slice() else {
            panic!("the deposit was sent once");
        };
        signed.clone()
    };
    assert_eq!(keccak256(&signed), transaction_hash);
    let recorded = payment.saved().swap;
    let [deposit] = recorded.transactions() else {
        panic!("only the deposit is recorded");
    };
    assert_eq!((deposit.hash, deposit.inclusion), (transaction_hash, None));
    assert_eq!(
        deposit
            .signed
            .as_ref()
            .map(PublicSwapSignedTransaction::raw),
        Some(&signed)
    );
    owner.shutdown().await;
    drop(owner);

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    payment.track(&restarted, &across, None).await.unwrap();
    payment.chain.lock().unwrap().rejects_sends = false;
    payment.track(&restarted, &across, None).await.unwrap();
    {
        let chain = payment.chain.lock().unwrap();
        assert_eq!(chain.sent, [signed.clone(), signed.clone(), signed]);
        assert!(matches!(
            chain.broadcasts.as_slice(),
            [(included, _)] if *included == transaction_hash
        ));
    }
    assert_eq!(payment.saved().swap.transactions().len(), 1);

    payment.store.stop_swap_use(payment.operation).unwrap();
    payment.mine();
    payment.track(&restarted, &across, None).await.unwrap();
    assert_eq!(payment.chain.lock().unwrap().sent.len(), 3);
    assert!(payment.saved().swap.observations().bridge_handoff.is_some());
    across_task.abort();
    payment.finish(restarted).await;
}

// No RPC answered the deposit's send, yet it reached the network, and a reorganization within
// the finality depth includes it below the head it was sent at. Its scan starts the finality
// depth below that head, so it finds the deposit there, and the nonce the deposit used rules
// nothing out.
#[tokio::test]
async fn a_deposit_reorganized_below_its_sending_head_is_found_not_ruled_out() {
    let (mut payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    payment.origin.finality_depth = 3;
    payment.chain.lock().unwrap().rejects_sends = true;
    let PublicSwapTransactionOutcome::HandedOff { transaction_hash } =
        payment.deposit(&owner).await.unwrap()
    else {
        panic!("the deposit is handed off");
    };
    assert_eq!(
        payment.saved().swap.transactions()[0].deposit_scan_from_block,
        Some(ORIGIN_HEAD - 3)
    );
    let included_at = ORIGIN_HEAD - 1;
    {
        let event = payment.deposited(payment.terms.input_amount, payment.terms.output_amount);
        let mut origin = payment.chain.lock().unwrap();
        let transaction = TxEnvelope::decode_2718(&mut &origin.sent[0][..]).unwrap();
        origin.chain.reorg(included_at);
        origin.chain.include(
            included_at,
            transaction,
            payment.source,
            true,
            vec![(payment.spoke_pool, event.encode_log_data())],
        );
        // Blocks up to one past the sending head are final, so a scan from that head would
        // reach the account's used nonce without the deposit.
        origin.chain.head = ORIGIN_HEAD + 4;
    }
    let handoff = owner
        .observe_public_swap_handoff(
            payment.operation,
            payment.id,
            &payment.origin,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            handoff.observation.block.number,
            handoff.observation.transaction_hash
        ),
        (included_at, Some(transaction_hash))
    );
    assert_eq!(payment.saved().swap.transactions()[0].ruled_out, None);
    payment.finish(owner).await;
}

// Another transaction of the Public account used the deposit's nonce, and no RPC took the
// deposit. It is ruled out only at a canonical finalized block such that every block from its
// boundary up to that one was inspected without it and the account's nonce there passed its
// own: not from a scan short of the other transaction, and not from a block that changed
// after the nonce read. The rule-out survives a restart and admits no late inclusion. Before
// it the swap takes no other deposit. After it the swap is back in preparation with its
// destination shield kept, and either deposits again under the next nonce or is cancelled.
#[tokio::test(start_paused = true)]
async fn a_deposit_whose_nonce_another_transaction_used_is_ruled_out() {
    for cancels in [false, true] {
        let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
        let (across, _reply, _lookups, across_task) = across_stub().await;
        let shield_state = payment.saved().record.payload_state(payment.shield);
        payment.chain.lock().unwrap().rejects_sends = true;
        let PublicSwapTransactionOutcome::HandedOff {
            transaction_hash: hash,
        } = payment.deposit(&owner).await.unwrap()
        else {
            panic!("the deposit is handed off");
        };
        assert!(payment.deposit(&owner).await.is_err());
        assert_eq!(payment.chain.lock().unwrap().sent.len(), 1);

        let mut other =
            alloy::rpc::types::TransactionRequest::default().to(Address::repeat_byte(0x99));
        other.chain_id = Some(DESTINATION_CHAIN);
        other.from = Some(payment.source);
        other.nonce = Some(0);
        other.gas = Some(21_000);
        other.max_fee_per_gas = Some(100);
        other.max_priority_fee_per_gas = Some(1);
        let other = payment
            .signer
            .sign_transaction_request(other, "another transaction")
            .await
            .unwrap();
        let other = TxEnvelope::decode_2718(&mut other.as_slice()).unwrap();
        let used_at = ORIGIN_HEAD + 7;
        {
            let mut origin = payment.chain.lock().unwrap();
            origin
                .chain
                .include(used_at, other, payment.source, true, Vec::new());
            origin.chain.head = used_at + 3;
        }
        let ruled_out = || payment.saved().swap.transactions()[0].ruled_out;

        // Eight blocks stop short of the other transaction, where the nonce is still unused.
        assert_eq!(
            owner
                .observe_public_swap_handoff(
                    payment.operation,
                    payment.id,
                    &payment.origin,
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            payment.saved().swap.transactions()[0].deposit_scan_from_block,
            Some(used_at)
        );
        assert_eq!(ruled_out(), None);
        // The next scan reaches the finalized head, whose block changes after the nonce read.
        let finalized = used_at + 2;
        payment.chain.lock().unwrap().reorg_on_nonce_read = Some(finalized);
        assert_eq!(
            owner
                .observe_public_swap_handoff(
                    payment.operation,
                    payment.id,
                    &payment.origin,
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            None
        );
        assert_eq!(ruled_out(), None);
        // The block after it is final and stays canonical.
        payment.mine();
        assert_eq!(
            owner
                .observe_public_swap_handoff(
                    payment.operation,
                    payment.id,
                    &payment.origin,
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .unwrap(),
            None
        );
        let evidence = SwapObservation {
            block: payment.chain.lock().unwrap().chain.block(finalized + 1),
            transaction_hash: None,
        };
        assert_eq!(ruled_out(), Some(evidence));
        let late = PublicSwapInclusion {
            observation: SwapObservation {
                block: evidence.block,
                transaction_hash: Some(hash),
            },
            succeeded: true,
            finalized: true,
        };
        assert!(matches!(
            payment
                .store
                .record_public_swap_inclusion(payment.operation, payment.id, hash, late),
            Err(ExecutorStoreError::InvalidRecord)
        ));

        owner.shutdown().await;
        drop(owner);
        let restarted = ExecutorOwner::new(
            1,
            payment.db.clone(),
            payment.view.clone(),
            payment.destination.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        payment.chain.lock().unwrap().rejects_sends = false;
        payment.track(&restarted, &across, None).await.unwrap();
        assert_eq!(payment.chain.lock().unwrap().sent.len(), 1);
        let preparing = payment.saved();
        assert_eq!(preparing.swap.transactions()[0].ruled_out, Some(evidence));
        assert!(!preparing.swap.is_finished(false, unix_now()));
        assert_eq!(preparing.record.public_swaps_to_track().count(), 1);

        if cancels {
            payment
                .store
                .cancel_public_swap(payment.operation, payment.id)
                .unwrap();
            assert_eq!(payment.saved().record.public_swaps_to_track().count(), 0);
        } else {
            let PublicSwapTransactionOutcome::Included {
                transaction_hash: second,
                ..
            } = payment.deposit(&restarted).await.unwrap()
            else {
                panic!("the second deposit is included");
            };
            let continued = payment.saved().swap;
            let [ruled, live] = continued.transactions() else {
                panic!("both deposits are recorded");
            };
            assert_eq!((ruled.hash, live.hash), (hash, second));
            assert_eq!(live.transaction.nonce, Some(1));
        }
        let after = payment.saved();
        assert_eq!(after.shields, [payment.shield]);
        assert_eq!(after.record.payload_state(payment.shield), shield_state);
        for request in &payment.chain.lock().unwrap().requests {
            assert!(
                !request["params"]
                    .to_string()
                    .contains(&alloy::hex::encode(hash))
            );
        }
        across_task.abort();
        payment.finish(restarted).await;
    }
}

// The deposit's receipt was seen, but a reorganization within the finality depth orphaned its
// block, and another transaction of the Public account took its nonce there. Its provisional
// inclusion stays recorded and doesn't hold the deposit live: the final block holds the other
// transaction instead, so the deposit is ruled out, and the swap deposits again under the next
// nonce with its destination shield kept.
#[tokio::test(start_paused = true)]
async fn a_deposit_whose_provisional_block_was_orphaned_for_its_nonce_is_ruled_out() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let shield_state = payment.saved().record.payload_state(payment.shield);
    let PublicSwapTransactionOutcome::Included {
        block_number,
        transaction_hash: hash,
    } = payment.deposit(&owner).await.unwrap()
    else {
        panic!("the deposit is included");
    };
    let provisional = payment.saved().swap.transactions()[0].inclusion.unwrap();
    assert!(!provisional.finalized);

    let mut other = alloy::rpc::types::TransactionRequest::default().to(Address::repeat_byte(0x99));
    other.chain_id = Some(DESTINATION_CHAIN);
    other.from = Some(payment.source);
    other.nonce = Some(0);
    other.gas = Some(21_000);
    other.max_fee_per_gas = Some(100);
    other.max_priority_fee_per_gas = Some(1);
    let other = payment
        .signer
        .sign_transaction_request(other, "another transaction")
        .await
        .unwrap();
    let other = TxEnvelope::decode_2718(&mut other.as_slice()).unwrap();
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.chain.reorg(block_number);
        origin
            .chain
            .include(block_number, other, payment.source, true, Vec::new());
        // The other transaction's block is final.
        origin.chain.head = block_number + 1;
        // Only observation's requests are checked below. The deposit's own step sent its hash.
        origin.requests.clear();
    }

    // Verifying the orphaned block takes one inspection, and the scan from the boundary then
    // reads three blocks up to it, all in one poll.
    assert_eq!(
        owner
            .observe_public_swap_handoff(
                payment.operation,
                payment.id,
                &payment.origin,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .unwrap(),
        None
    );
    let evidence = SwapObservation {
        block: payment.chain.lock().unwrap().chain.block(block_number),
        transaction_hash: None,
    };
    let observed = payment.saved();
    let [ruled] = observed.swap.transactions() else {
        panic!("only the deposit is recorded");
    };
    assert_eq!(
        (ruled.hash, ruled.inclusion, ruled.ruled_out),
        (hash, Some(provisional), Some(evidence))
    );
    for request in &payment.chain.lock().unwrap().requests {
        assert!(
            !request["params"]
                .to_string()
                .contains(&alloy::hex::encode(hash))
        );
    }

    owner.shutdown().await;
    drop(owner);
    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let PublicSwapTransactionOutcome::Included {
        transaction_hash: second,
        ..
    } = payment.deposit(&restarted).await.unwrap()
    else {
        panic!("the second deposit is included");
    };
    let after = payment.saved();
    let [ruled, live] = after.swap.transactions() else {
        panic!("both deposits are recorded");
    };
    assert_eq!((ruled.hash, live.hash), (hash, second));
    assert_eq!(live.transaction.nonce, Some(1));
    assert_eq!(after.shields, [payment.shield]);
    assert_eq!(after.record.payload_state(payment.shield), shield_state);
    payment.finish(restarted).await;
}
