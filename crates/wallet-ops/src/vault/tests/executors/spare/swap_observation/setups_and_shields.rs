use super::*;

// A pending setup's pass reads the account at the latest block too, on the same endpoint and
// about the same account, and returns what it shows. The record keeps the confirmed read.
#[tokio::test]
async fn a_pending_setup_pass_reads_the_latest_block_once_and_records_nothing() {
    let fixture = Fixture::start_pending().await;
    let other_code = Bytes::from(
        [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            Address::repeat_byte(0xdd).as_slice(),
        ]
        .concat(),
    );
    let latest_reads = |requests: &[Value]| {
        requests
            .iter()
            .filter(|request| {
                request["method"] == "eth_getBlockByNumber" && request["params"][0] == "latest"
            })
            .count()
    };
    // The confirmed block 14 shows the setup's nonce 0 unconsumed. The latest block 15 shows
    // the delegation and nonce 1, the nonce unconsumed, or another delegation.
    for (nonce, code, included) in [
        (1, None, true),
        (0, None, false),
        (1, Some(other_code), false),
    ] {
        let before = {
            let mut chain = fixture.chain.lock().unwrap();
            chain.head = 15;
            chain.nonces = vec![(15, nonce)];
            chain.head_code = code;
            chain.rpc_requests.len()
        };
        let observed = fixture
            .owner
            .observe_swap_setup(
                fixture.operation,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .unwrap();
        assert_eq!(observed.status, crate::SwapSetupStatus::Pending);
        assert_eq!(
            observed
                .inclusion
                .map(|inclusion| (inclusion.block(), inclusion.nonce())),
            included.then_some((15, U256::ONE))
        );
        let chain = fixture.chain.lock().unwrap();
        let pass = &chain.rpc_requests[before..];
        assert_eq!(latest_reads(pass), 1);
        assert!(
            pass.iter()
                .filter(|request| request["method"] == "eth_getCode")
                .all(|request| request["params"][0] == json!(EXECUTOR))
        );
        let at_confirmed = ExecutorNonceObservation::new(chain.block(14), U256::ZERO);
        drop(chain);
        let record = fixture.record();
        assert_eq!(record.nonce_observation(), Some(at_confirmed));
        assert_eq!(
            record.payload_state(fixture.setup),
            Some(ExecutorPayloadState::Pending)
        );
    }

    // A confirmed read resolves the setup, and the pass reads no latest block.
    let before = {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.nonces = vec![(14, 1)];
        chain.head_code = None;
        chain.rpc_requests.len()
    };
    let observed = fixture
        .owner
        .observe_swap_setup(
            fixture.operation,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(matches!(
        observed.status,
        crate::SwapSetupStatus::Delegated(_)
    ));
    assert_eq!(observed.inclusion, None);
    assert_eq!(
        latest_reads(&fixture.chain.lock().unwrap().rpc_requests[before..]),
        0
    );
    fixture.finish().await;
}

// An order on an included setup names the record's confirmed observation, which stays the floor
// of its status reads. A pre-hook that ran outside a trade in a block below the latest block
// read at signing is still found.
#[tokio::test]
async fn an_order_on_an_included_setup_is_observed_from_the_confirmed_block() {
    let fixture = Fixture::start_pending().await;
    let receiver = Address::repeat_byte(0x77);
    let delivery = SwapDelivery::External { receiver };
    let attempt = fixture.attempt_after(
        0,
        100,
        BUY,
        delivery,
        SwapApprovedBounds {
            post_hook_gas_limit: None,
            ..swap_bounds()
        },
        None,
        1,
    );
    // Signed while the latest block 15 showed the setup included.
    let record = fixture
        .store
        .record_swap_attempt_on_included_setup(fixture.operation, attempt, U256::ONE)
        .unwrap();
    let floor = record
        .issued()
        .iter()
        .find(|payload| payload.purpose() == ExecutorPayloadPurpose::SwapPreHook)
        .unwrap()
        .context()
        .observed()
        .block()
        .number;
    assert_eq!(floor, 13);
    // The setup and the pre-hook run in block 14.
    fund(&fixture, 14);
    let record = fixture.observe(17, floor).await;
    assert!(matches!(
        state(&record, 0),
        SwapOrderState::PreHookOnly { .. }
    ));
    assert_eq!(
        record.payload_state(fixture.setup),
        Some(ExecutorPayloadState::Resolved)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn an_approved_or_restricted_setup_is_not_offered_for_another_swap() {
    let fixture = Fixture::start().await;
    let refusal = |operation| {
        fixture
            .owner
            .swap_account_refusal(operation, SwapAccountRole::Source)
            .unwrap()
    };
    // A setup whose approved order still awaits placement belongs to that swap.
    assert!(listed(&fixture.owner, fixture.operation).is_none());
    assert_eq!(
        refusal(fixture.operation),
        Some(SwapAccountRefusal::ClaimedByAnotherSwap)
    );
    // Each restriction keeps an account out of the list for its own reason: a delegate this
    // chain doesn't accept, an account kept for recovery, and one registered in Public.
    let [foreign, public] = std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
    for (operation, delegate, address) in [
        (foreign, Address::repeat_byte(7), Address::repeat_byte(0x71)),
        (public, fixture.delegate, Address::repeat_byte(0x72)),
    ] {
        fixture
            .store
            .reserve(operation, delegate, Some("Private swap"), &[])
            .unwrap();
        fixture.store.bind_address(operation, address).unwrap();
    }
    fixture
        .store
        .register_public_account(public, Address::repeat_byte(0x72))
        .unwrap();
    let recovered = fixture
        .store
        .restore_index(40, Address::repeat_byte(0x73), fixture.delegate, &[])
        .unwrap()
        .operation();
    assert_eq!(
        [foreign, public, recovered].map(refusal),
        [
            Some(SwapAccountRefusal::UnsupportedDelegate),
            Some(SwapAccountRefusal::RegisteredInPublic),
            Some(SwapAccountRefusal::RecoveryOnly),
        ]
    );
    assert_eq!(
        fixture.owner.swap_account_candidates().unwrap(),
        [] as [crate::SwapAccountCandidate; 0]
    );
    fixture.finish().await;
}

// A source preparation cancelled after its setup was sent keeps its account until a confirmed
// read resolves the setup and every payload the account signed. Only then is the account
// offered to a new swap as a source, and fresh checks still read its delegation.
#[tokio::test]
async fn a_cancelled_source_is_offered_again_once_its_setup_and_work_resolve() {
    let fixture = Fixture::start_pending().await;
    let first = SwapUseId::first(fixture.operation);
    fixture
        .owner
        .record_swap_approval(
            fixture.operation,
            first,
            SwapApproval {
                bounds: swap_bounds(),
                price_verified: Some(false),
                price_acknowledged: true,
                delivery: SwapDelivery::Reshield,
                tokens: None,
                accounts: None,
            },
        )
        .unwrap();
    assert_eq!(
        fixture
            .store
            .cancel_swap_use(fixture.operation, first)
            .unwrap()
            .source,
        crate::vault::SwapUseRelease::IssuedWorkRemains
    );
    let refusal = |role| {
        fixture
            .owner
            .swap_account_refusal(fixture.operation, role)
            .unwrap()
    };
    // The setup is only in the head block, above the confirmed one.
    assert_eq!(
        refusal(SwapAccountRole::Source),
        Some(SwapAccountRefusal::SetupUnconfirmed)
    );
    assert!(listed(&fixture.owner, fixture.operation).is_none());

    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.head = 16;
        chain.nonces = vec![(14, 1)];
    }
    fixture
        .owner
        .reconcile_account(
            fixture.operation,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(refusal(SwapAccountRole::Source), None);
    assert!(listed(&fixture.owner, fixture.operation).is_some());
    // The cancelled use itself never continues, and the account takes no destination use.
    assert_eq!(
        swap_account_refusal(
            &fixture.record(),
            1,
            SwapAccountRole::Source,
            SwapAccountUse::Claimed(first),
            SwapAdmissionEvidence::Recorded,
        ),
        Some(SwapAccountRefusal::RecoveryOnly)
    );
    assert_eq!(
        refusal(SwapAccountRole::Destination { token: BUY }),
        Some(SwapAccountRefusal::RecoveryOnly)
    );

    // Fresh checks refuse another delegation at the confirmed block and sign nothing.
    let issued = fixture.record().issued().to_vec();
    fixture.chain.lock().unwrap().delegate = Address::repeat_byte(0xdd);
    assert_eq!(
        reuse_refusal(&fixture.owner, fixture.operation, 15).await,
        SwapAccountRefusal::SetupUnconfirmed
    );
    fixture.chain.lock().unwrap().delegate = fixture.delegate;
    let reused = fixture
        .owner
        .reuse_swap_account(
            fixture.operation,
            15,
            SwapAccountRole::Source,
            SwapAccountUse::New,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(reused.is_reused());
    assert_eq!(fixture.record().issued(), issued.as_slice());

    // A recovery signed after the cancellation retires the account for good.
    record_cancellation(&fixture, Vec::new(), &sell_note(5));
    assert_eq!(
        refusal(SwapAccountRole::Source),
        Some(SwapAccountRefusal::RecoveryOnly)
    );
    assert_eq!(
        reuse_refusal(&fixture.owner, fixture.operation, 15).await,
        SwapAccountRefusal::RecoveryOnly
    );
    fixture.finish().await;
}

#[tokio::test]
async fn swap_delivery_tolerates_dust_retains_shield_evidence_and_reopens_on_reorg() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    let order = uid(0, 100);

    let record = fixture.observe(31, 20).await;
    assert_eq!(state(&record, 0), SwapOrderState::Open);
    assert!(fixture.reserved(&record));
    assert!(matches!(
        fixture.owner.input_locks(&[sell_note(3)]).unwrap().as_slice(),
        [lock] if matches!(lock.reason(), crate::ExecutorInputLockReason::OrderOpen { .. })
    ));

    // One settlement runs the pre-hook, trades, and runs the post-hook.
    let settlement_tx = B256::repeat_byte(0x40);
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(order)));
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_975)));
        chain.add_logs(40, settlement_tx, logs);
        chain.nonces.push((40, 3));
        // A large gift makes delivery ambiguous on the first page. Later only dust remains.
        chain.buy_balance.push((41, BUY_AMOUNT));
    }
    let record = fixture.observe(46, 35).await;
    assert_eq!(state(&record, 0), SwapOrderState::NotDelivered);
    fixture.chain.lock().unwrap().buy_balance.push((46, 1));
    // The next page contains no settlement logs. Delivery still uses the saved shield.
    let record = fixture.observe(47, 46).await;
    let settled = fixture.chain.lock().unwrap().block(40);
    let observed = record.swap().unwrap().orders()[0].observations();
    assert_eq!(state(&record, 0), SwapOrderState::Done);
    assert_eq!(
        (observed.pre_hook_executed, observed.traded),
        (
            Some(SwapObservation {
                block: settled,
                transaction_hash: Some(settlement_tx)
            }),
            Some(SwapObservation {
                block: settled,
                transaction_hash: Some(settlement_tx)
            })
        )
    );
    // The executed amounts come from the order's Trade log and stay with the retained trade.
    assert_eq!(
        observed.trade_amounts,
        Some(crate::vault::SwapTradeAmounts {
            sell_amount: U256::from(SELL_AMOUNT),
            buy_amount: U256::from(BUY_AMOUNT),
            fee_amount: U256::ZERO,
            settlement_gas_used: None,
            settlement_effective_gas_price: None,
            executed_fee: None,
            executed_fee_token: None,
        })
    );
    // The retained shield keeps the credited amount with the fee its event charged.
    assert_eq!(
        observed
            .shielded
            .map(|shield| (shield.private_amount, shield.fee)),
        Some((U256::from(9_975), Some(U256::from(25))))
    );
    // Delivery rests on the balance at the confirmed block, after the settlement.
    assert_eq!(
        observed.delivered,
        Some(SwapObservation {
            block: fixture.chain.lock().unwrap().block(46),
            transaction_hash: Some(settlement_tx)
        })
    );
    assert!(
        fixture.reserved(&record),
        "spent inputs stay protected while private sync catches up"
    );
    let available = fixture
        .owner
        .inputs_for_record(vec![sell_note(3), sell_note(4)], &record)
        .unwrap();
    assert_eq!(
        available
            .iter()
            .map(|input| input.position)
            .collect::<Vec<_>>(),
        vec![4],
        "reusing the account cannot select its earlier swap's spent note"
    );

    // A reorg drops the settlement: every observation reopens and the inputs are reserved again.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.reorg(38);
        chain.nonces = vec![(11, 1)];
    }
    let record = fixture.observe(47, 40).await;
    assert_eq!(
        record.swap().unwrap().orders()[0].observations(),
        SwapOrderObservations::default()
    );
    assert_eq!(state(&record, 0), SwapOrderState::Open);
    assert!(fixture.reserved(&record));

    // On the new chain a funded post-hook ran before the payout: its shield is in the
    // settlement, but the proceeds stay in the executor, so the swap is not delivered.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_975)));
        logs.push((fixture.settlement, trade_log(order)));
        chain.add_logs(50, B256::repeat_byte(0x50), logs);
        chain.nonces.push((50, 3));
        chain.buy_balance.push((50, BUY_AMOUNT));
    }
    let record = fixture.observe(56, 48).await;
    let observed = record.swap().unwrap().orders()[0].observations();
    assert_eq!(state(&record, 0), SwapOrderState::NotDelivered);
    assert_eq!(
        observed.traded.map(|traded| traded.block),
        Some(fixture.chain.lock().unwrap().block(50))
    );
    assert!(observed.delivered.is_none());
    fixture.finish().await;
}

// A completed order must not hide a sibling of its swap use that still awaits its post-hook
// shield: the page holding that shield is read even after every hook nonce of the use passed.
#[tokio::test]
async fn a_later_shield_completes_a_second_trade_of_the_same_swap_use() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    // One settlement runs the first attempt's pre-hook, trades, and runs its post-hook.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(uid(0, 100))));
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_975)));
        chain.add_logs(40, B256::repeat_byte(0x40), logs);
        chain.nonces.push((40, 3));
    }
    let record = fixture.observe(46, 35).await;
    assert_eq!(state(&record, 0), SwapOrderState::Done);

    // A second attempt of the same use signs its hooks at the next nonces, 3 and 4.
    fixture.record_attempt(1, 200);
    let record = fixture.record();
    let orders = record.swap().unwrap().orders();
    assert!(orders[1].use_id().is_some() && orders[1].use_id() == orders[0].use_id());
    // Its settlement runs the pre-hook and trades, but the post-hook does not run: the
    // nonce stops at the post-hook's and the proceeds stay in the executor.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(uid(1, 200))));
        chain.add_logs(50, B256::repeat_byte(0x50), logs);
        chain.nonces.push((50, 4));
        chain.buy_balance.push((50, BUY_AMOUNT));
    }
    let record = fixture.observe(56, 46).await;
    assert_eq!(state(&record, 1), SwapOrderState::NotDelivered);

    // The post-hook runs later on its own. Every hook nonce of the use is now consumed.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_logs(
            60,
            B256::repeat_byte(0x60),
            vec![(fixture.railgun, post_hook_shield_log(1, 9_975))],
        );
        chain.nonces.push((60, 5));
        chain.buy_balance.push((60, 1));
    }
    let record = fixture.observe(66, 56).await;
    assert_eq!(state(&record, 1), SwapOrderState::Done);
    assert_eq!(state(&record, 0), SwapOrderState::Done);
    fixture.finish().await;
}

#[tokio::test]
async fn a_shield_below_the_approved_private_minimum_does_not_complete_the_swap() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(uid(0, 100))));
        // Matching post-hook evidence and an empty executor are insufficient when the
        // credited amount is below the minimum the user approved.
        logs.push((fixture.railgun, post_hook_shield_log(0, 9_974)));
        chain.add_logs(40, B256::repeat_byte(0x40), logs);
        chain.nonces.push((40, 3));
    }
    let record = fixture.observe(46, 35).await;
    assert_ne!(state(&record, 0), SwapOrderState::Done);
    assert!(
        record.swap().unwrap().orders()[0]
            .observations()
            .delivered
            .is_none()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn reconciling_an_external_swap_delivers_on_its_trade_without_reading_balances() {
    let fixture = Fixture::start().await;
    let receiver = Address::repeat_byte(0x77);
    fixture.record_delivery_attempt(0, 100, BUY, SwapDelivery::External { receiver }, None);
    // One settlement runs the pre-hook and pays the receiver. Only the pre-hook takes a nonce.
    let settlement_tx = B256::repeat_byte(0x40);
    {
        let mut chain = fixture.chain.lock().unwrap();
        let mut logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect::<Vec<_>>();
        logs.push((fixture.settlement, trade_log(uid(0, 100))));
        chain.add_logs(40, settlement_tx, logs);
        chain.nonces.push((40, 2));
        chain.rpc_requests.clear();
    }
    let record = fixture.observe(46, 35).await;
    let settled = Some(SwapObservation {
        block: fixture.chain.lock().unwrap().block(40),
        transaction_hash: Some(settlement_tx),
    });
    let observed = record.swap().unwrap().orders()[0].observations();
    assert_eq!(state(&record, 0), SwapOrderState::Done);
    assert_eq!(
        (observed.traded, observed.delivered, observed.undelivered),
        (settled, settled, None)
    );
    assert!(record.swap().unwrap().admits_attempt());
    assert!(!reads_a_balance(
        &fixture.chain.lock().unwrap().rpc_requests
    ));
    fixture.finish().await;
}

#[tokio::test]
async fn an_external_pre_hook_without_a_trade_recovers_only_the_sell_token() {
    let fixture = Fixture::start().await;
    let receiver = Address::repeat_byte(0x77);
    fixture.record_delivery_attempt(
        0,
        35,
        Address::ZERO,
        SwapDelivery::External { receiver },
        None,
    );
    {
        let mut chain = fixture.chain.lock().unwrap();
        let logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect();
        chain.add_logs(30, B256::repeat_byte(0x30), logs);
        chain.nonces.push((30, 2));
    }
    let record = fixture.observe(41, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::PreHookOnly { expired: true }
    );
    // The expired order can't fill, so recovery only resets the sell token's approval.
    let calls = crate::swap_recovery_calls(
        &record,
        &swap_profile(),
        EXECUTOR,
        &[(SELL, U256::from(SELL_AMOUNT))],
        at_block(41),
    )
    .unwrap();
    let [reset] = calls.as_slice() else {
        panic!("recovery resets only the sell token's approval");
    };
    let approval =
        broadcaster_core::contracts::railgun::approveCall::abi_decode(&reset.data).unwrap();
    assert_eq!(
        (reset.to, approval.spender, approval.amount),
        (SELL, swap_profile().vault_relayer(), U256::ZERO)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn an_unchanged_open_order_reads_no_logs_until_its_fill_moves() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    let log_queries = || fixture.chain.lock().unwrap().log_queries;

    // The pre-hook's nonce is unused, nothing is filled, and `validTo` is ahead.
    let record = fixture.observe(31, 20).await;
    assert_eq!(state(&record, 0), SwapOrderState::Open);
    assert_eq!(log_queries(), 0);

    // A solver skips the pre-hook and settles from funds the executor already holds:
    // the nonce stays unused and only the fill shows the trade.
    fixture.chain.lock().unwrap().add_logs(
        33,
        B256::repeat_byte(0x33),
        vec![(fixture.settlement, trade_log(uid(0, 100)))],
    );
    let record = fixture.observe(36, 31).await;
    assert_eq!(state(&record, 0), SwapOrderState::Traded);
    assert!(log_queries() > 0);
    fixture.finish().await;

    // Past `validTo` the settlement may clear an order's fill even though it traded, so
    // a zero fill no longer rules out a trade in pages up to `validTo`.
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 28);
    let log_queries = || fixture.chain.lock().unwrap().log_queries;
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_logs(
            25,
            B256::repeat_byte(0x25),
            vec![(fixture.settlement, trade_log(uid(0, 28)))],
        );
        chain.fill_cleared.push((uid(0, 28), 30));
        chain.head = 41;
    }
    // A historical page before the trade records the expiry.
    let record = fixture.observe_page(20..25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    // A page wholly after `validTo` can't hold a trade, and its logs aren't read.
    let before = log_queries();
    fixture.observe_page(40..41).await;
    assert_eq!(log_queries(), before);
    // The trade's page is still read and finds it.
    let record = fixture.observe_page(25..30).await;
    assert_eq!(state(&record, 0), SwapOrderState::Traded);
    fixture.finish().await;
}

// A page past the block an endpoint read the account at fails as unconfirmed, also when private
// sync's newer resolution withheld that read, and succeeds once the endpoint confirms the page.
#[tokio::test]
async fn a_page_past_the_accounts_confirmed_block_waits_for_it() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    // The account is read at block 19, one short of the page.
    fixture.chain.lock().unwrap().head = 20;
    let error = fixture
        .owner
        .observe_swap(
            fixture.operation,
            20..21,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(error.is::<crate::SwapPageUnconfirmed>(), "{error:#}");
    assert!(fixture.record().nonce_observation().is_none());
    // Private sync resolved the pre-hook's nonce in block 20, so the read at 19 is withheld.
    fund(&fixture, 20);
    fixture
        .store
        .record_synced(
            fixture.operation,
            Some(ExecutorNonceWatermark::new(U256::from(2), 20)),
            20,
        )
        .unwrap();
    let error = fixture
        .owner
        .observe_swap(
            fixture.operation,
            20..21,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(error.is::<crate::SwapPageUnconfirmed>(), "{error:#}");
    fixture.observe(21, 20).await;
    fixture.finish().await;
}
