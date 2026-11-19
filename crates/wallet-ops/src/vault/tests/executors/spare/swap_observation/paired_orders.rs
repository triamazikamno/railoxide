use super::*;

// Both orders' pre-hooks share the proof and the nonce. The proof's effects in the block where
// the nonce passes show that the account was funded, and not which pre-hook ran.
#[tokio::test]
async fn shared_proof_effects_record_one_funding_and_name_neither_pre_hook() {
    let fixture = Fixture::start().await;
    let [_, second] = record_pair(&fixture, [100, 110]);
    let (swap_use, k) = (SwapUseId::first(fixture.operation), U256::ONE);
    let transaction = fund(&fixture, 30);
    let record = fixture.observe(36, 25).await;
    let funding = SwapObservation {
        block: fixture.chain.lock().unwrap().block(30),
        transaction_hash: Some(transaction),
    };
    let swap = record.swap().unwrap();
    let [funded] = swap.fundings() else {
        panic!("one funding for the use and the shared nonce");
    };
    assert_eq!(
        (funded.use_id, funded.pre_hook_nonce, funded.observation),
        (swap_use, k, funding)
    );
    for order in swap.orders() {
        let observed = order.observations();
        assert_eq!(
            (
                observed.pre_hook_executed,
                observed.pre_hook_dead,
                observed.traded
            ),
            (None, None, None)
        );
        assert_eq!(swap.funding(order), Some(funding));
        assert_eq!(
            swap_order_state(swap, order),
            SwapOrderState::PreHookOnly { expired: false }
        );
    }
    assert_eq!(
        (record.swap_hook_winner(k), record.resolved_swap_hook(k)),
        (None, None)
    );

    // Whichever pre-hook ran took the shared nonce, so nothing more is signed at it, and a
    // retry waits as for an executed pre-hook.
    let refusal = |swap_use| {
        swap_account_refusal(
            &record,
            1,
            SwapAccountRole::Source,
            swap_use,
            SwapAdmissionEvidence::Fresh,
        )
    };
    assert_eq!(
        refusal(SwapAccountUse::SpeedUp(SwapSpeedUp {
            swap_use,
            order: second,
            pre_hook_nonce: k,
        })),
        Some(SwapAccountRefusal::SpeedUpNonceUsed)
    );
    assert_eq!(
        refusal(SwapAccountUse::Claimed(swap_use)),
        Some(SwapAccountRefusal::PreviousOrderLive)
    );
    assert!(matches!(
        fixture.store.record_swap_attempt(
            fixture.operation,
            fixture.attempt(2, 120, BUY, SwapDelivery::Reshield, swap_bounds(), None),
        ),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    assert!(
        fixture.reserved(&record),
        "the spent notes wait for private sync"
    );

    // The funded pair has nothing left to find, so a later page's logs aren't read, and a
    // reopened store holds the same funding.
    let log_queries = || fixture.chain.lock().unwrap().log_queries;
    let before = log_queries();
    let later = fixture.observe(37, 36).await;
    assert_eq!(log_queries(), before);
    assert_eq!(later.swap(), record.swap());
    assert_eq!(stored(&fixture), later);

    // A reorg of the funding's block removes it, and both orders are open again.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.reorg(29);
        chain.nonces = vec![(11, 1)];
        chain.nullifiers_spent = None;
    }
    let reopened = fixture.observe(38, 30).await;
    assert_eq!(reopened.swap().unwrap().fundings(), [] as [SwapFunding; 0]);
    assert_eq!(order_states(&reopened), vec![SwapOrderState::Open; 2]);
    assert!(fixture.reserved(&reopened));
    fixture.finish().await;
}

// The deadline guard reverts a pre-hook after its order's `validTo`. Effects in a block past
// the first order's `validTo` and within the second's can only be the second pre-hook's.
#[tokio::test]
async fn effects_past_one_orders_valid_to_name_the_other_orders_pre_hook() {
    let fixture = Fixture::start().await;
    record_pair(&fixture, [28, 100]);
    let transaction = fund(&fixture, 30);
    let record = fixture.observe(36, 25).await;
    let ran = SwapObservation {
        block: fixture.chain.lock().unwrap().block(30),
        transaction_hash: Some(transaction),
    };
    let swap = record.swap().unwrap();
    let [first, second] = swap.orders() else {
        panic!("the speed-up is the second order");
    };
    assert_eq!(
        (
            first.observations().pre_hook_executed,
            second.observations().pre_hook_executed
        ),
        (None, Some(ran))
    );
    assert_eq!(
        swap.fundings()
            .iter()
            .map(|funding| funding.observation)
            .collect::<Vec<_>>(),
        vec![ran]
    );
    assert_eq!(
        record.swap_hook_winner(U256::ONE),
        Some(second.pre_hook().payload())
    );
    assert_eq!(
        swap_order_state(swap, second),
        SwapOrderState::PreHookOnly { expired: false }
    );
    // The first order had expired when the funding ran, so it ended without sharing it.
    assert_eq!(
        swap_order_state(swap, first),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    fixture.finish().await;
}

// The nonce passes both pre-hooks' nonce with none of the proof's effects in the page, first
// with the proof's notes unspent and then with them reading as spent. Neither shows a funding.
#[tokio::test]
async fn a_consumed_nonce_or_spent_notes_alone_record_no_funding() {
    for spent in [false, true] {
        let fixture = Fixture::start().await;
        record_pair(&fixture, [100, 110]);
        {
            let mut chain = fixture.chain.lock().unwrap();
            chain.nonces.push((30, 2));
            chain.nullifiers_spent = spent.then_some(30);
        }
        let record = fixture.observe(36, 25).await;
        let swap = record.swap().unwrap();
        assert!(swap.fundings().is_empty(), "spent: {spent}");
        assert!(
            swap.orders()
                .iter()
                .all(|order| order.observations().pre_hook_executed.is_none()),
            "spent: {spent}"
        );
        assert_eq!(
            (
                record.swap_hook_winner(U256::ONE),
                record.resolved_swap_hook(U256::ONE)
            ),
            (None, None),
            "spent: {spent}"
        );
        // Unspent notes show that no pre-hook ran, and nothing says what took the nonce.
        let expected = if spent {
            SwapOrderState::Open
        } else {
            SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown)
        };
        assert_eq!(order_states(&record), vec![expected; 2], "spent: {spent}");
        fixture.finish().await;
    }
}

// A funded pair whose orders both expire untraded needs recovery, like one order whose
// pre-hook ran. The single order runs beside the pair, so that admission after the recovery is
// the same for both.
#[tokio::test]
async fn a_funded_pair_that_expires_untraded_recovers_like_one_executed_pre_hook() {
    let (profile, k) = (swap_profile(), U256::ONE);
    let mut admissions = Vec::new();
    for overlap in [false, true] {
        let fixture = Fixture::start().await;
        let swap_use = SwapUseId::first(fixture.operation);
        fixture.record_attempt(0, 40);
        if overlap {
            fixture
                .store
                .record_swap_attempt(
                    fixture.operation,
                    speed_up_attempt(&fixture, 1, 44, uid(0, 40)),
                )
                .unwrap();
        }
        let orders = 1 + usize::from(overlap);
        fund(&fixture, 30);
        let record = fixture.observe(36, 25).await;
        assert_eq!(
            order_states(&record),
            vec![SwapOrderState::PreHookOnly { expired: false }; orders],
            "overlap: {overlap}"
        );

        // A finalized block passes every `validTo` without a trade.
        let record = fixture.observe(47, 36).await;
        let stranded = vec![SwapOrderState::PreHookOnly { expired: true }; orders];
        assert_eq!(order_states(&record), stranded, "overlap: {overlap}");
        let fundings = record.swap().unwrap().fundings().to_vec();
        assert_eq!(fundings.len(), 1, "overlap: {overlap}");
        // A reopened store holds the funding and the states.
        let kept = stored(&fixture);
        assert_eq!(kept, record, "overlap: {overlap}");
        assert_eq!(order_states(&kept), stranded, "overlap: {overlap}");

        // No order can fill, so recovery only resets the sell token's approval, and nothing
        // is left to cancel.
        let calls = crate::swap_recovery_calls(
            &kept,
            &profile,
            EXECUTOR,
            &[(SELL, U256::from(SELL_AMOUNT))],
            at_block(47),
        )
        .unwrap();
        let [reset] = calls.as_slice() else {
            panic!("recovery resets only the sell token's approval");
        };
        let approval =
            broadcaster_core::contracts::railgun::approveCall::abi_decode(&reset.data).unwrap();
        assert_eq!(
            (reset.to, approval.spender, approval.amount),
            (SELL, profile.vault_relayer(), U256::ZERO)
        );
        for nonce in [k, U256::from(2)] {
            assert!(
                crate::swap_cancellation_admitted(&kept, nonce, &profile, at_block(47)).is_err(),
                "overlap: {overlap}"
            );
        }

        // The recovery runs at the next nonce and shields the sell token back, and private
        // sync receives its note.
        let request = ShieldRequest {
            preimage: CommitmentPreimage {
                npk: B256::repeat_byte(0x70),
                token: TokenData::erc20(SELL),
                value: U120::from(SELL_AMOUNT),
            },
            ..post_hook_shield(0)
        };
        let shield = Call {
            to: EXECUTOR,
            value: U256::ZERO,
            data: shieldCall {
                _shieldRequests: vec![request.clone()],
            }
            .abi_encode()
            .into(),
        };
        let recovery = B256::repeat_byte(0x3a);
        fixture
            .store
            .record_issued(
                fixture.operation,
                IssuedExecutorPayload::new(
                    U256::from(2),
                    fixture.delegate,
                    recovery,
                    ExecutorPayloadPurpose::Recovery,
                    ExecutorPayloadContext::new(
                        execute(
                            vec![private_transaction(0x31, 0x32)],
                            vec![reset.clone(), shield],
                            2,
                        ),
                        record.nonce_observation().unwrap(),
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        fixture.chain.lock().unwrap().nonces.push((50, 3));
        let received = synced(vec![railgun_wallet::WalletUtxo::new(Utxo::new(
            request.preimage.note_with_random([0; 16]),
            0,
            50,
            UtxoSource {
                tx_hash: B256::repeat_byte(0x3c),
                block_number: 50,
                block_timestamp: timestamp(50),
            },
            UtxoCommitmentKind::Shield,
        ))]);
        let record = fixture.observe_synced(56, 47, received.clone()).await;
        assert_eq!(
            crate::payload_outcome(
                &record,
                recovery,
                &crate::ExecutorAttributionEvidence {
                    railgun: fixture.railgun,
                    sync: Some(&received),
                    invalidated_orders: None,
                },
            ),
            Some(crate::ExecutorPayloadOutcome::Executed),
            "overlap: {overlap}"
        );
        let swap = record.swap().unwrap();
        assert_eq!(swap.fundings(), fundings, "overlap: {overlap}");
        if overlap {
            // The recovery names neither pre-hook.
            assert!(
                swap.orders()
                    .iter()
                    .all(|order| order.observations().pre_hook_executed.is_none())
            );
            assert_eq!(
                (record.swap_hook_winner(k), record.resolved_swap_hook(k)),
                (None, None)
            );
        }
        let admission = |swap_use| {
            swap_account_refusal(
                &record,
                1,
                SwapAccountRole::Source,
                swap_use,
                SwapAdmissionEvidence::Fresh,
            )
        };
        let mut states = order_states(&record);
        states.dedup();
        admissions.push((
            states,
            swap.admits_attempt(),
            admission(SwapAccountUse::Claimed(swap_use)),
            admission(SwapAccountUse::New),
        ));
        fixture.finish().await;
    }
    // After the recovery the pair reads and is admitted as the single order is.
    assert_eq!(admissions[1], admissions[0]);
}

// A settlement receipt after the shared funding holds one order's trade and its post-hook's
// private credit. The trade names the order that traded and no pre-hook.
#[tokio::test]
async fn a_receipt_trade_after_shared_funding_completes_the_use_and_names_no_pre_hook() {
    for trader in [0_usize, 1] {
        let fixture = Fixture::start().await;
        let valid_to = [100, 110];
        let uids = record_pair(&fixture, valid_to);
        fund(&fixture, 30);
        fixture.observe(36, 25).await;
        let attempt = u8::try_from(trader).unwrap();
        let logs = settlement_logs(&fixture, attempt, valid_to[trader]);
        fixture.chain.lock().unwrap().nonces.push((40, 3));
        let traded = settle_retry(&fixture, uids[trader], 40, logs)
            .await
            .unwrap();

        let record = fixture.record();
        let swap = record.swap().unwrap();
        let (won, other) = (&swap.orders()[trader], &swap.orders()[1 - trader]);
        assert_eq!(
            (won.observations().traded, won.observations().delivered),
            (Some(traded), Some(traded)),
            "trader {trader}"
        );
        assert_eq!(
            won.observations()
                .settlement_credit
                .map(|credit| credit.private_amount),
            Some(U256::from(BUY_AMOUNT - 24)),
            "trader {trader}"
        );
        assert_eq!(
            swap_order_state(swap, won),
            SwapOrderState::Done,
            "trader {trader}"
        );
        assert_eq!(
            (other.observations().traded, other.observations().delivered),
            (None, None),
            "trader {trader}"
        );
        // The trade ran against the funding, whichever pre-hook made it.
        assert_eq!(swap.fundings().len(), 1, "trader {trader}");
        assert!(
            swap.orders()
                .iter()
                .all(|order| order.observations().pre_hook_executed.is_none()),
            "trader {trader}"
        );
        // One full trade exhausts the exact allowance, so the other order can't fill and the
        // use is complete.
        assert_eq!(
            (swap_order_state(swap, other), swap.funding(other)),
            (SwapOrderState::Superseded, None),
            "trader {trader}"
        );
        assert!(
            swap.admits_attempt(),
            "trader {trader}: the completed use admits another attempt"
        );
        fixture.finish().await;
    }
}

// The shared notes stay reserved while either pre-hook can run. At an unused nonce they are
// released once both pre-hooks are dead at finality. At a resolved nonce they wait for private
// sync, whichever of the orders or a cancellation resolved it.
#[tokio::test]
async fn a_pairs_notes_wait_for_both_pre_hooks_to_die_or_for_private_sync() {
    let fixture = Fixture::start().await;
    record_pair(&fixture, [28, 34]);
    let retry = || {
        fixture.store.record_swap_attempt(
            fixture.operation,
            fixture.attempt(2, 100, BUY, SwapDelivery::Reshield, swap_bounds(), None),
        )
    };
    let expired = SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired);
    // The first order's `validTo` passed at finality. The speed-up's pre-hook can still run.
    let record = fixture.observe(31, 21).await;
    assert_eq!(order_states(&record), vec![expired, SwapOrderState::Open]);
    assert!(fixture.reserved(&record));
    assert!(matches!(
        retry(),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    // Both passed with the nonce unused: the notes are released, and a retry is admitted by
    // the rule for any retry and reserves them again.
    let record = fixture.observe(37, 31).await;
    assert_eq!(order_states(&record), vec![expired; 2]);
    assert!(!fixture.reserved(&record));
    let retried = retry().unwrap();
    assert!(fixture.reserved(&retried));
    fixture.finish().await;

    for resolver in ["fill", "cancellation"] {
        let fixture = Fixture::start().await;
        let [first, second] = record_pair(&fixture, [100, 110]);
        let (record, expected) = if resolver == "fill" {
            // The speed-up fills in a settlement that also runs the shared proof.
            settle_on_chain(&fixture, 30, second, 1, 9_975);
            (
                fixture.observe(36, 25).await,
                vec![SwapOrderState::Superseded, SwapOrderState::Done],
            )
        } else {
            // One cancellation takes the shared nonce and invalidates both orders. Private
            // sync shows its fee note spent in the transaction the broadcaster reported.
            let fee = sell_note(4);
            let cancellation = record_cancellation(
                &fixture,
                vec![
                    invalidation(fixture.settlement, first),
                    invalidation(fixture.settlement, second),
                ],
                &fee,
            );
            let sent = B256::repeat_byte(0x3b);
            fixture
                .store
                .record_submission(fixture.operation, cancellation, sent)
                .unwrap();
            {
                let mut chain = fixture.chain.lock().unwrap();
                chain.nonces.push((30, 2));
                chain.invalidated.extend([(first, 30), (second, 30)]);
            }
            let mut spent = railgun_wallet::WalletUtxo::new(fee);
            spent.spent = Some(UtxoSource {
                tx_hash: sent,
                block_number: 30,
                block_timestamp: timestamp(30),
            });
            (
                fixture.observe_synced(36, 25, synced(vec![spent])).await,
                vec![SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation); 2],
            )
        };
        assert_eq!(order_states(&record), expected, "{resolver}");
        assert!(
            fixture.reserved(&record),
            "{resolver}: private sync hasn't reached the resolving block"
        );
        // A restart keeps both attempts' notes out of reach while waiting.
        let restarted = restarted_owner(&fixture);
        let kept = stored(&fixture);
        assert!(fixture.reserved(&kept), "{resolver}");
        assert_eq!(
            restarted
                .inputs_for_record(vec![sell_note(3), sell_note(5)], &kept)
                .unwrap()
                .iter()
                .map(|input| input.position)
                .collect::<Vec<_>>(),
            vec![5],
            "{resolver}"
        );
        restarted.shutdown().await;
        // Once private sync passes the read that resolved the nonce, its spent status is all
        // that keeps a note from being selected.
        let resolved_at = record.nonce_watermark().unwrap().block();
        let settled = fixture
            .store
            .record_synced(fixture.operation, None, resolved_at)
            .unwrap();
        assert!(!fixture.reserved(&settled), "{resolver}");
        assert_eq!(
            fixture
                .owner
                .inputs_for_record(vec![sell_note(3)], &settled)
                .unwrap()
                .len(),
            1,
            "{resolver}"
        );
        fixture.finish().await;
    }
}

// One cancellation at the shared nonce invalidates both live orders and pays its fee from
// other notes. It is reported as executed only on evidence of its own: not when either order's
// trade lands first, and not when the nonce resolves with nothing to tell what took it.
#[tokio::test]
async fn one_cancellation_invalidates_both_live_orders_and_is_claimed_only_on_evidence() {
    let (profile, k) = (swap_profile(), U256::ONE);
    // The order whose trade lands before the cancellation. With `None` the nonce passes
    // with no trade, no private effects and neither order invalidated.
    for trader in [Some(0_usize), Some(1), None] {
        let fixture = Fixture::start().await;
        let uids = record_pair(&fixture, [100, 110]);
        let record = fixture.record();
        let calls = crate::swap_recovery_calls(
            &record,
            &profile,
            EXECUTOR,
            &[(SELL, U256::from(SELL_AMOUNT))],
            at_block(20),
        )
        .unwrap();
        let [cancel_first, cancel_second, reset] = calls.as_slice() else {
            panic!("two invalidations and the approval reset");
        };
        let invalidated =
            GPv2Settlement::invalidateOrderCall::abi_decode(&cancel_first.data).unwrap();
        assert_eq!(
            (cancel_first.to, &invalidated.orderUid[..]),
            (fixture.settlement, &uids[0].0[..])
        );
        assert_invalidates_then_resets(
            &[cancel_second.clone(), reset.clone()],
            fixture.settlement,
            uids[1],
        );
        // The fee review's call bound, two per order, covers the batch.
        assert!(calls.len() <= 2 * record.swap().unwrap().orders().len());
        crate::swap_cancellation_admitted(&record, k, &profile, at_block(20)).unwrap();
        // The fee comes from a note neither pre-hook reserves.
        let fee = sell_note(4);
        assert_eq!(
            fixture
                .owner
                .inputs_for_recovery(vec![sell_note(3), fee.clone()], &record)
                .unwrap()
                .iter()
                .map(|input| input.position)
                .collect::<Vec<_>>(),
            vec![4]
        );
        let cancellation = record_cancellation(&fixture, calls.clone(), &fee);

        let expected = if let Some(trader) = trader {
            settle_on_chain(
                &fixture,
                30,
                uids[trader],
                u8::try_from(trader).unwrap(),
                9_975,
            );
            let mut states = vec![SwapOrderState::Superseded; 2];
            states[trader] = SwapOrderState::Done;
            states
        } else {
            fixture.chain.lock().unwrap().nonces.push((30, 2));
            vec![SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown); 2]
        };
        let record = fixture.observe(36, 25).await;
        // The record follows the verified trade, or ends both attempts without a cause.
        assert_eq!(order_states(&record), expected, "trader {trader:?}");
        assert_eq!(
            (record.swap_hook_winner(k), record.resolved_swap_hook(k)),
            (None, None),
            "trader {trader:?}"
        );
        let attribution = fixture.owner.attribution(&record).unwrap();
        assert_eq!(
            crate::payload_outcome(&record, cancellation, &attribution.evidence()),
            Some(crate::ExecutorPayloadOutcome::Resolved),
            "trader {trader:?}: the nonce is resolved and the cancellation is not named"
        );
        fixture.finish().await;
    }
}

// Either attempt's post-hook shields the full bought balance to the same recipient, so the
// speed-up's shield can deliver the first order's trade. The executor's balance, the private
// credit and the traded order's approved minimum decide, as for the order's own shield.
#[tokio::test]
async fn either_attempts_shield_delivers_the_first_orders_trade_above_its_minimum() {
    // The attempt whose post-hook ran, what it credited, and whether that delivers.
    for (shielder, credit, delivers) in [(1_u8, 9_975, true), (0, 9_975, true), (1, 9_974, false)] {
        let case = format!("shielder {shielder}, credit {credit}");
        let fixture = Fixture::start().await;
        let [first, _] = record_pair(&fixture, [100, 110]);
        settle_on_chain(&fixture, 40, first, shielder, credit);
        let record = fixture.observe(46, 35).await;
        let orders = record.swap().unwrap().orders();
        let (traded, speed_up) = (orders[0].observations(), orders[1].observations());
        assert!(
            traded.traded.is_some() && speed_up.traded.is_none(),
            "{case}"
        );
        assert_eq!(
            orders[usize::from(shielder)]
                .observations()
                .shielded
                .map(|shield| shield.private_amount),
            Some(U256::from(credit)),
            "{case}"
        );
        assert_eq!(traded.delivered.is_some(), delivers, "{case}");
        assert_eq!(
            state(&record, 0) == SwapOrderState::Done,
            delivers,
            "{case}"
        );
        assert_eq!(traded.undelivered, None, "{case}");
        fixture.finish().await;
    }
}
