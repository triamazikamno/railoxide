use super::*;

#[tokio::test]
async fn public_signing_needs_every_order_closed_at_the_fresh_confirmed_block() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 40);
    let order = uid(0, 40);

    // The nonce passes both hooks, which resolves them, but the signed order can still fill.
    fixture.chain.lock().unwrap().nonces.push((20, 3));
    let (record, closed) = orders_closed_at(&fixture, 22).await;
    assert!(!record.has_unresolved_issued_work());
    assert!(!closed.unwrap());

    // A full fill closes it, read from the settlement without a saved trade observation.
    fixture.chain.lock().unwrap().add_logs(
        23,
        B256::repeat_byte(0x23),
        vec![(fixture.settlement, trade_log(order))],
    );
    let (filled, closed) = orders_closed_at(&fixture, 25).await;
    assert!(closed.unwrap());
    assert_eq!(state(&filled, 0), SwapOrderState::Open);

    // A reorg removes the fill while the hook nonces stay spent. The earlier confirmed
    // block no longer counts, and the fresh one shows the order open again.
    fixture.chain.lock().unwrap().reorg(23);
    assert!(
        fixture
            .owner
            .swap_orders_closed(
                &fixture.config,
                &filled,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    let (_, closed) = orders_closed_at(&fixture, 25).await;
    assert!(!closed.unwrap());

    // Invalidation and expiry each close the order.
    fixture.chain.lock().unwrap().invalidated.push((order, 26));
    let (_, closed) = orders_closed_at(&fixture, 28).await;
    assert!(closed.unwrap());
    fixture.chain.lock().unwrap().invalidated.clear();
    let (expired, closed) = orders_closed_at(&fixture, 42).await;
    assert!(closed.unwrap());

    // The confirmed block can't be read.
    fixture.chain.lock().unwrap().head = 30;
    assert!(
        fixture
            .owner
            .swap_orders_closed(
                &fixture.config,
                &expired,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    fixture.finish().await;
}

/// A stealth account's confirmed read, its swap's order observation and a signing inspection
/// go only over the account's operation client, never over the session's shared RPC client.
#[tokio::test]
async fn stealth_account_reads_use_only_the_account_client() {
    use crate::rpc_broker::tests::{spawn_recording_rpc_mock, tagged_reqwest_client};
    let shared = || {
        HttpContext::with_rpc_client_for_tests(
            tagged_reqwest_client("shared"),
            crate::WalletNetworkMode::Direct,
        )
    };
    let account = crate::OperationHttpClient::for_tests(
        tagged_reqwest_client("account"),
        crate::OperationNetworkIsolation::Dedicated,
    );
    let only_account = |routes: &[Option<String>]| {
        assert_ne!(routes, [] as [Option<String>; 0]);
        assert!(
            routes
                .iter()
                .all(|route| route.as_deref() == Some("account")),
            "{routes:?}"
        );
    };

    // The swap's account: its confirmed read, then its order observation.
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 100);
    fixture.chain.lock().unwrap().head = 31;
    let served = fixture.chain.clone();
    let (endpoint, recorded, server) = spawn_recording_rpc_mock(Arc::new(move |request: Value| {
        served.lock().unwrap().respond(&request)
    }))
    .await;
    let mut config = fixture.config.clone();
    config.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
    let owner = ExecutorOwner::new(
        1,
        fixture.db.clone(),
        fixture.view.clone(),
        config,
        shared(),
    )
    .unwrap();
    owner
        .reconcile_account(fixture.operation, &account)
        .await
        .unwrap();
    owner
        .observe_swap(fixture.operation, 20..31, &account)
        .await
        .unwrap();
    let routes: Vec<_> = recorded
        .lock()
        .unwrap()
        .iter()
        .map(|(route, _)| route.clone())
        .collect();
    only_account(&routes);
    server.abort();
    owner.shutdown().await;
    drop(owner);
    fixture.finish().await;

    // A fresh account's signing inspection while it is prepared.
    let rpc = crate::vault::tests::executors::spare::Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        crate::vault::tests::executors::spare::chain(&rpc),
        shared(),
    )
    .unwrap();
    owner
        .prepare_operation(
            ExecutorOperationId::random().unwrap(),
            crate::vault::tests::executors::spare::delivery(),
            &crate::vault::tests::executors::spare::swap_setup::password(),
            &[],
            None,
            &account,
        )
        .await
        .unwrap();
    only_account(&rpc.state.routes.lock().unwrap());
    owner.shutdown().await;
    drop(owner);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn expired_pre_hook_releases_inputs_at_finality_and_stays_released_after_restart() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 28);

    // `validTo` passed at the confirmed block while the nonce stayed unused.
    let record = fixture.observe(31, 21).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert!(!fixture.reserved(&record));

    let requests = fixture.chain.lock().unwrap().rpc_methods.len();
    let restarted = restarted_owner(&fixture);
    let retained = restarted
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == fixture.operation)
        .unwrap();
    assert_eq!(retained.nonce_observation(), record.nonce_observation());
    assert!(
        !fixture.reserved(&retained),
        "finalized release survives restart"
    );
    assert_eq!(
        restarted.input_locks(&[sell_note(3)]).unwrap(),
        [] as [crate::ExecutorInputLock; 0]
    );
    assert_eq!(fixture.chain.lock().unwrap().rpc_methods.len(), requests);
    restarted.shutdown().await;
    fixture.finish().await;
}

#[tokio::test]
async fn checking_an_expired_orders_lock_observes_the_order_and_frees_its_notes() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 28);
    let notes = [sell_note(3)];
    // `validTo` passed, but no observation recorded the pre-hook's death yet.
    let locks = fixture.owner.input_locks(&notes).unwrap();
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].operation(), fixture.operation);

    fixture.chain.lock().unwrap().head = 31;
    let lock = fixture
        .owner
        .check_input_lock(
            fixture.operation,
            30,
            &notes,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert!(lock.is_none());
    assert_eq!(
        state(&fixture.record(), 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert_eq!(
        fixture.owner.input_locks(&notes).unwrap(),
        [] as [crate::ExecutorInputLock; 0]
    );
    fixture.finish().await;
}

#[tokio::test]
async fn invalidated_order_names_a_cancellation_without_a_reported_transaction_or_sync() {
    for previously_unknown in [false, true] {
        let fixture = Fixture::start().await;
        fixture.record_attempt(0, 200);
        let order = uid(0, 200);
        let observed = fixture.record().nonce_observation().unwrap();
        fixture
            .store
            .record_issued(
                fixture.operation,
                IssuedExecutorPayload::new(
                    U256::ONE,
                    fixture.delegate,
                    B256::repeat_byte(0x39),
                    ExecutorPayloadPurpose::Recovery,
                    ExecutorPayloadContext::new(
                        execute(
                            vec![private_transaction(0x31, 0x32)],
                            vec![invalidation(fixture.settlement, order)],
                            1,
                        ),
                        observed,
                        vec![ExecutorInputIdentity::from_utxo(&sell_note(4))],
                    ),
                ),
            )
            .unwrap();
        fixture.chain.lock().unwrap().nonces.push((30, 2));
        if previously_unknown {
            let record = fixture.observe(36, 25).await;
            assert_eq!(
                state(&record, 0),
                SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown)
            );
            // The unresolved post-hook makes the next page worth reading, so page selection
            // skips the filled-amount probe. The Unknown cause still needs that order state.
            fixture.chain.lock().unwrap().nonces.push((31, 3));
        }
        fixture.chain.lock().unwrap().invalidated.push((order, 30));
        let record = fixture.observe(37, 25).await;
        assert_eq!(
            state(&record, 0),
            SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
        );
        assert_eq!(
            crate::swap_invalidation(&record, &swap_profile(), at_block(37)).unwrap(),
            None
        );
        fixture.finish().await;
    }
}

#[tokio::test]
async fn cancellation_and_an_older_post_hook_end_attempts_but_a_copied_shield_does_not() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let first = uid(0, 200);

    // A broadcaster-funded cancellation at the pre-hook's nonce invalidates the order. The
    // broadcaster reported its transaction, which pays the fee with a note of its own.
    let observed = fixture.record().nonce_observation().unwrap();
    let cancellation = B256::repeat_byte(0x39);
    let sent = B256::repeat_byte(0x3b);
    let fee = sell_note(4);
    fixture
        .store
        .record_issued(
            fixture.operation,
            IssuedExecutorPayload::new(
                U256::ONE,
                fixture.delegate,
                cancellation,
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(
                    execute(
                        vec![private_transaction(0x31, 0x32)],
                        vec![invalidation(fixture.settlement, first)],
                        1,
                    ),
                    observed,
                    vec![ExecutorInputIdentity::from_utxo(&fee)],
                ),
            ),
        )
        .unwrap();
    fixture
        .store
        .record_submission(fixture.operation, cancellation, sent)
        .unwrap();
    // No read finds the transaction: only the nonce shows that something ran.
    fixture.chain.lock().unwrap().nonces.push((30, 2));
    let record = fixture.observe(36, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown)
    );
    // Until the cause is known the order counts as fillable, so a retry invalidates it again.
    assert_eq!(
        crate::swap_invalidation(&record, &swap_profile(), at_block(36)).unwrap(),
        Some(first)
    );
    // Private sync shows the fee note spent in the recorded transaction.
    let mut spent = railgun_wallet::WalletUtxo::new(fee);
    spent.spent = Some(UtxoSource {
        tx_hash: sent,
        block_number: 30,
        block_timestamp: timestamp(30),
    });
    let record = fixture.observe_synced(36, 25, synced(vec![spent])).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)
    );
    // The pre-hook's nonce is consumed, so its notes wait for private sync to pass that read.
    assert!(fixture.reserved(&record));
    let resolved_at = record.nonce_watermark().unwrap().block();
    let record = fixture
        .store
        .record_synced(fixture.operation, None, resolved_at)
        .unwrap();
    assert!(!fixture.reserved(&record));

    // The retry signs its pre-hook at k + 1, the nonce of the first attempt's post-hook.
    fixture.record_attempt(1, 300);
    let record = fixture.record();
    assert!(fixture.reserved(&record));

    // A shield with the older post-hook's public request, without that post-hook taking the
    // nonce, doesn't identify it.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_logs(
            58,
            B256::repeat_byte(0x58),
            vec![(fixture.railgun, post_hook_shield_log(0, 9_975))],
        );
    }
    let record = fixture.observe(61, 57).await;
    assert_eq!(state(&record, 1), SwapOrderState::Open);

    // The older post-hook takes the retry's nonce inside a settlement.
    {
        let mut chain = fixture.chain.lock().unwrap();
        chain.add_logs(
            62,
            B256::repeat_byte(0x62),
            vec![(fixture.railgun, post_hook_shield_log(0, 9_975))],
        );
        chain.nonces.push((62, 3));
    }
    let record = fixture.observe(66, 61).await;
    assert_eq!(
        state(&record, 1),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::OlderPostHook)
    );
    // The retry's order can still fill; the cancelled one can't. The next retry's pre-hook
    // invalidates exactly the retry's order.
    let profile =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .unwrap()
            .swap_profile()
            .unwrap();
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(timestamp(66));
    assert_eq!(
        crate::swap_invalidation(&record, &profile, now).unwrap(),
        Some(uid(1, 300))
    );
    // No direct call to the executor won nonce k + 1, but the nonce read past it resolves it,
    // so the next retry is admitted at k + 2.
    fixture.record_attempt(2, 400);
    let record = fixture.record();
    assert_eq!(
        record.swap().unwrap().orders()[2].pre_hook().nonce(),
        U256::from(3)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn a_received_shield_names_the_recovery_over_a_cancellation_at_the_pre_hooks_nonce() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let first = uid(0, 200);
    let observed = fixture.record().nonce_observation().unwrap();
    // A cancellation and a recovery that also shields the sell token, both at the pre-hook's
    // nonce and neither with a recorded transaction.
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
    let invalidate = invalidation(fixture.settlement, first);
    for (hash, calls) in [
        (0x39, vec![invalidate.clone()]),
        (0x3a, vec![invalidate, shield]),
    ] {
        fixture
            .store
            .record_issued(
                fixture.operation,
                IssuedExecutorPayload::new(
                    U256::ONE,
                    fixture.delegate,
                    B256::repeat_byte(hash),
                    ExecutorPayloadPurpose::Recovery,
                    ExecutorPayloadContext::new(
                        execute(vec![private_transaction(0x31, 0x32)], calls, 1),
                        observed,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
    }
    fixture.chain.lock().unwrap().nonces.push((30, 2));
    // Nothing tells the two apart, and the cancellation is not named for lack of a shield.
    let record = fixture.observe(36, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Unknown)
    );

    let received = railgun_wallet::WalletUtxo::new(Utxo::new(
        request.preimage.note_with_random([0; 16]),
        0,
        50,
        UtxoSource {
            tx_hash: B256::repeat_byte(0x3c),
            block_number: 30,
            block_timestamp: timestamp(30),
        },
        UtxoCommitmentKind::Shield,
    ));
    let record = fixture.observe_synced(36, 25, synced(vec![received])).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Recovery)
    );
    fixture.finish().await;
}

fn hook_payload(
    record: &ExecutorRecord,
    purpose: ExecutorPayloadPurpose,
) -> &IssuedExecutorPayload {
    record
        .issued()
        .iter()
        .find(|payload| payload.purpose() == purpose)
        .unwrap()
}

#[tokio::test]
async fn recovery_invalidates_an_open_order_but_an_expired_pre_hook_is_not_outstanding() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 28);
    let profile = swap_profile();
    let k = U256::ONE;

    // The open order can fill, and its pre-hook holds the current nonce k.
    let record = fixture.record();
    let pre_hook = hook_payload(&record, ExecutorPayloadPurpose::SwapPreHook);
    assert!(record.is_outstanding_at(pre_hook, k));
    assert!(record.has_competing_payloads());
    // The post-hook at k + 1 is the swap exception, not recorded state ahead of the chain.
    assert!(!record.records_future_nonce(k));
    let calls = crate::swap_recovery_calls(
        &record,
        &profile,
        EXECUTOR,
        &[(SELL, U256::from(SELL_AMOUNT))],
        at_block(20),
    )
    .unwrap();
    assert_invalidates_then_resets(&calls, fixture.settlement, uid(0, 28));
    crate::swap_cancellation_admitted(&record, k, &profile, at_block(20)).unwrap();

    // `validTo` passed at finality with k unused: nothing is outstanding at k, so recovery
    // warns of no competing payload and adds no call for the dead pre-hook.
    let record = fixture.observe(31, 21).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)
    );
    assert!(
        !record
            .issued()
            .iter()
            .any(|payload| record.is_outstanding_at(payload, k))
    );
    assert!(!record.has_competing_payloads());
    assert!(
        crate::swap_recovery_calls(&record, &profile, EXECUTOR, &[], at_block(31))
            .unwrap()
            .is_empty()
    );
    assert!(crate::swap_cancellation_admitted(&record, k, &profile, at_block(31)).is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn a_settled_pre_hook_beats_a_cancellation_and_recovery_is_offered() {
    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let observed = fixture.record().nonce_observation().unwrap();
    let cancellation = |hash: u8, inputs: Vec<ExecutorInputIdentity>| {
        IssuedExecutorPayload::new(
            U256::ONE,
            fixture.delegate,
            B256::repeat_byte(hash),
            ExecutorPayloadPurpose::Recovery,
            ExecutorPayloadContext::new(
                execute(vec![private_transaction(0x31, 0x32)], Vec::new(), 1),
                observed,
                inputs,
            ),
        )
    };
    // The cancellation's fee can't spend the note the swap's own pre-hook reserves.
    assert!(matches!(
        fixture.store.record_issued(
            fixture.operation,
            cancellation(0x38, vec![fixture.input.clone()])
        ),
        Err(ExecutorStoreError::InputReserved)
    ));
    let fee = ExecutorInputIdentity::from_utxo(&sell_note(4));
    let cancelled = fixture
        .store
        .record_issued(fixture.operation, cancellation(0x39, vec![fee.clone()]))
        .unwrap();
    assert!(cancelled.reserved_inputs().contains(&fee));

    // A solver runs the pre-hook inside a settlement before the cancellation confirms.
    {
        let mut chain = fixture.chain.lock().unwrap();
        let logs = private_logs(PRE_HOOK_NULLIFIER, PRE_HOOK_COMMITMENT)
            .into_iter()
            .map(|data| (fixture.railgun, data))
            .collect();
        chain.add_logs(30, B256::repeat_byte(0x30), logs);
        chain.nonces.push((30, 2));
    }
    let record = fixture.observe(36, 25).await;
    assert_eq!(
        state(&record, 0),
        SwapOrderState::PreHookOnly { expired: false }
    );
    assert!(
        record.reserved_inputs().contains(&fee),
        "the loser's fee note waits for private sync like every note at the consumed nonce"
    );
    let resolved_at = record.nonce_watermark().unwrap().block();
    let record = fixture
        .store
        .record_synced(fixture.operation, None, resolved_at)
        .unwrap();
    assert!(
        !record.reserved_inputs().contains(&fee),
        "the loser's fee note is released"
    );

    // Recovery competes only with the post-hook at the current nonce k + 1, and invalidates
    // the order that can still fill with the executor's funds.
    let post_hook = hook_payload(&record, ExecutorPayloadPurpose::SwapPostHook);
    assert!(record.is_outstanding_at(post_hook, U256::from(2)));
    assert!(!record.records_future_nonce(U256::from(2)));
    let calls = crate::swap_recovery_calls(
        &record,
        &swap_profile(),
        EXECUTOR,
        &[(SELL, U256::from(SELL_AMOUNT))],
        at_block(36),
    )
    .unwrap();
    assert_invalidates_then_resets(&calls, fixture.settlement, uid(0, 200));
    fixture.finish().await;
}

#[tokio::test]
async fn cancellation_fee_selection_excludes_the_swaps_reserved_notes() {
    use railgun_wallet::tx::{
        BuildError, MixedPrivateActionRequest, MixedPrivateSend, MixedPrivateSendRole,
        SelectedInputIdentity,
    };

    let fixture = Fixture::start().await;
    fixture.record_attempt(0, 200);
    let record = fixture.record();
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: fixture.railgun,
    };
    // The broadcaster fee is paid in the swap's sell token.
    let fee = MixedPrivateActionRequest {
        adapter: None,
        executor_calls: Vec::new(),
        private_sends: vec![MixedPrivateSend {
            token_address: SELL,
            amount: U256::ONE,
            recipient: broadcaster_core::crypto::railgun::AddressData {
                master_public_key: U256::ONE,
                viewing_public_key: [1; 32],
            },
            role: MixedPrivateSendRole::Other,
        }],
        public_unshields: Vec::new(),
        relay_actions: None,
        min_gas_price: 1,
        verify_proof: false,
        spend_up_to: false,
        rebuild: None,
    };
    let reserved = sell_note(3);
    let inputs = fixture
        .owner
        .inputs_for_recovery(vec![reserved.clone()], &record)
        .unwrap();
    assert!(matches!(
        builder.preview_mixed_private_action_plan(&inputs, &fee),
        Err(BuildError::InsufficientBalance(_) | BuildError::InsufficientFeeTokenBalance(_))
    ));
    let spare = sell_note(4);
    let inputs = fixture
        .owner
        .inputs_for_recovery(vec![reserved.clone(), spare.clone()], &record)
        .unwrap();
    assert_eq!(
        builder
            .preview_mixed_private_action_plan(&inputs, &fee)
            .unwrap()
            .selected_inputs,
        vec![SelectedInputIdentity::from_utxo(&spare)]
    );
    // A release frees the notes for other operations, but the swap's own cancellation still
    // competes with its pre-hook for them.
    fixture.owner.release_input_lock(fixture.operation).unwrap();
    let released = fixture.record();
    assert!(!fixture.reserved(&released));
    let inputs = fixture
        .owner
        .inputs_for_recovery(vec![reserved, spare.clone()], &released)
        .unwrap();
    assert_eq!(
        inputs
            .iter()
            .map(|input| input.position)
            .collect::<Vec<_>>(),
        vec![4]
    );
    fixture.finish().await;
}
