use super::*;

#[tokio::test]
async fn each_swap_setup_gets_its_own_executor_and_authorizes_only_that_executor() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let first = owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let second = owner
        .prepare_swap_setup(
            ExecutorOperationId::random().unwrap(),
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_ne!(first.context().executor, second.context().executor);
    assert!(
        owner
            .prepare_swap_setup(
                operation,
                broadcaster(delegate),
                setup_approval(WETH, USDC, SwapDelivery::Reshield),
                None,
                &password(),
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .is_err(),
        "a new swap never takes over a recorded swap's executor"
    );
    let resumed = owner
        .resume_swap_setup(
            operation,
            broadcaster(delegate),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(resumed.context().executor, first.context().executor);

    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            Address::repeat_byte(0x33),
            U256::from(9),
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let executor = resumed.context().executor;
    // The fee-only setup: one private transaction and no actions.
    let call = railgun_wallet::TransactionCall {
        to: executor,
        data: RelayAdapt7702::executeCall {
            _transactions: vec![Transaction {
                proof: SnarkProof::default(),
                merkleRoot: B256::ZERO,
                nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
                commitments: Vec::new(),
                boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
                unshieldPreimage: CommitmentPreimage::empty(),
            }],
            _actionData: RelayAdapt7702ActionData {
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls: Vec::new(),
            },
            _nonce: resumed.context().execution_nonce,
            _signature: Bytes::new(),
        }
        .abi_encode()
        .into(),
    };
    let issued = owner
        .issue_operation(
            &resumed,
            &call,
            std::slice::from_ref(&input),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let Some([authorization]) = issued.transaction().authorization_list.as_deref() else {
        panic!("the setup carries exactly one delegation authorization");
    };
    assert_eq!(authorization.recover_authority().unwrap(), executor);
    assert_ne!(
        authorization.recover_authority().unwrap(),
        second.context().executor
    );
    assert_eq!(authorization.inner().address, delegate);
    assert_eq!(issued.transaction().transaction_type, Some(4));

    // An unconfirmed setup can select another broadcaster and issue a replacement
    // at the same nonce, reusing its fee note instead of demanding a second note.
    let mut another = broadcaster(delegate);
    another.railgun_address = "0zk-another-broadcaster".into();
    another.fees_id = "new-fees".into();
    let replacement = owner
        .resume_swap_setup(
            operation,
            another.clone(),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    replacement.require_broadcaster(&another).unwrap();
    assert_eq!(replacement.context(), resumed.context());
    assert!(
        owner
            .available_inputs(vec![input.clone()])
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        owner
            .inputs_for_preparation(vec![input.clone()], &replacement)
            .unwrap()
            .len(),
        1
    );
    let mut replacement_call = call;
    let mut decoded = RelayAdapt7702::executeCall::abi_decode(&replacement_call.data).unwrap();
    decoded._actionData.minGasLimit = U256::ONE;
    replacement_call.data = decoded.abi_encode().into();
    let replaced = owner
        .issue_operation(
            &replacement,
            &replacement_call,
            std::slice::from_ref(&input),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_ne!(replaced.payload_hash(), issued.payload_hash());
    let record = owner
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(record.issued().len(), 2);
    assert_eq!(
        record.reserved_inputs(),
        vec![ExecutorInputIdentity::from_utxo(&input)]
    );
    owner
        .cancel_swap_use(None, operation, SwapUseId::first(operation))
        .unwrap();
    assert!(owner.swap_setup_preview(operation).is_err());
    assert!(
        owner
            .resume_swap_setup(
                operation,
                another,
                &password(),
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    // A preparation retained by an in-flight setup must not issue more payloads after stop.
    decoded._actionData.minGasLimit = U256::from(2);
    replacement_call.data = decoded.abi_encode().into();
    assert!(
        owner
            .issue_operation(
                &replacement,
                &replacement_call,
                &[input],
                &password(),
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    let stopped = owner
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert!(stopped.is_swap_setup_stopped());
    assert_eq!(stopped.issued(), record.issued());
    assert_eq!(stopped.reserved_inputs(), record.reserved_inputs());
    owner.shutdown().await;
    drop(owner);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

/// A setup job whose newly reserved account was used before goes on with the next one. The
/// used account is retired and released from the swap, and the swap use moves to the
/// replacement with its approval, so the setup that is prepared is the replacement's and is
/// admitted up to the fee limit approved with the swap. Nothing is issued for the used one.
#[tokio::test]
async fn a_used_new_account_is_replaced_and_the_swap_sets_up_the_next_one() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let candidate = broadcaster(chain.accepted_executor_profile().unwrap().delegate());
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let mut approval = setup_approval(WETH, USDC, SwapDelivery::Reshield);
    approval.bounds.source_setup_fee = Some(setup_fee_limit(2_000));
    // An earlier swap's preparation derives the next account's address, which the chain then
    // reports as used. The account after it is unused.
    owner
        .prepare_swap_setup(
            ExecutorOperationId::random().unwrap(),
            candidate.clone(),
            approval.clone(),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let used = store.spare().unwrap().unwrap().address().unwrap();
    rpc.state.used.lock().unwrap().insert(used);

    let operation = ExecutorOperationId::random().unwrap();
    let swap_use = SwapUseId::first(operation);
    let mut replaced = Vec::new();
    let mut accounts =
        SwapAccountReplacements::new(&owner, None, operation, swap_use, |from, replacement| {
            replaced.push((from, replacement));
        });
    let prepared = owner
        .prepare_swap_setup(
            operation,
            candidate.clone(),
            approval.clone(),
            None,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await;
    let prepared = accounts
        .prepare_setup(
            SwapUseSide::Source,
            prepared,
            &candidate,
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let replacement = accounts.operation();
    drop(accounts);
    assert_ne!(replacement, operation);
    assert_eq!(
        replaced,
        [(
            operation,
            SwapAccountReplacement {
                operation: replacement,
                swap_use,
                replacement,
            }
        )]
    );
    assert_eq!(prepared.operation(), replacement);
    assert_ne!(prepared.context().executor, used);

    let record = |operation| {
        owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };
    let retired = record(operation);
    assert_eq!(retired.address(), Some(used));
    assert!(retired.is_retired() && retired.swap_uses().is_empty() && retired.issued().is_empty());
    let account = record(replacement);
    assert_eq!(
        (account.active_swap_use(), account.is_retired()),
        (Some(swap_use), false)
    );
    assert_eq!(
        account
            .swap_use(swap_use)
            .and_then(|claimed| claimed.approval()),
        Some(&approval)
    );
    // The replacement's setup is paid within the limit approved with the swap, and no more.
    owner
        .require_swap_source_setup_fee(replacement, candidate.token, U256::from(2_000))
        .unwrap();
    assert!(
        owner
            .require_swap_source_setup_fee(replacement, candidate.token, U256::from(2_001))
            .is_err()
    );
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

/// A setup job that holds only the swap's own account, because the destination's setup is
/// already out, still saves an approval that names a replacement. The source of a claimed
/// private Bridge pair is found used when the job prepares it again, or when it submits its
/// setup. Either way the saved approval binds the replacement's address before anything is
/// handed off for it, so the first order's account check admits the pair, and the destination
/// account's binding, its issued setup and the approved terms stay as they were.
#[tokio::test]
async fn a_replaced_source_is_bound_in_the_approval_without_the_destination_owner() {
    for at_submission in [false, true] {
        let rpc = Rpc::start().await;
        let (root, db, vault) = desktop_store_with_vault();
        let view = Arc::new(import_wallet_with_metadata(
            &vault,
            TEST_WALLET_ID,
            "Wallet",
        ));
        let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
        let candidate = broadcaster(origin_chain.accepted_executor_profile().unwrap().delegate());
        let destination_delegate = destination_chain
            .accepted_executor_profile()
            .unwrap()
            .delegate();
        let mut destination_candidate = broadcaster(destination_delegate);
        destination_candidate.chain_id = DESTINATION_CHAIN;
        let owner = |generation, chain| {
            ExecutorOwner::new(
                generation,
                db.clone(),
                view.clone(),
                chain,
                HttpContext::direct_for_tests(),
            )
            .unwrap()
        };
        let (operation, destination_operation) = (
            ExecutorOperationId::random().unwrap(),
            ExecutorOperationId::random().unwrap(),
        );
        let swap_use = SwapUseId::first(operation);
        let authorization = password();
        let destination_authorization = authorization.for_destination().unwrap();

        // An earlier job claimed and derived both accounts, and the destination's setup is out.
        let (origin, destination) = (owner(0, origin_chain.clone()), owner(0, destination_chain));
        let prepared = prepare_swap_pair(
            &origin,
            Some(&destination),
            SwapPairPreparation {
                use_id: swap_use,
                source: SwapAccountChoice::New(operation),
                destination: Some(SwapAccountChoice::New(destination_operation)),
                candidate: Some(candidate.clone()),
                destination_candidate: Some(destination_candidate),
                approval: private_bridge_approval(),
                authorization: &authorization,
                destination_authorization: Some(&destination_authorization),
                client: crate::OperationHttpClient::direct_for_tests(),
            },
        )
        .await
        .unwrap();
        let (saved, mut side) = (prepared.approval, prepared.origin);
        let approved = saved.accounts.unwrap();
        let used = approved.source.address.unwrap();
        origin.shutdown().await;
        destination.shutdown().await;
        drop((origin, destination));
        let destination_store =
            ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
        let signed_at =
            ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
        destination_store
            .record_account_read(destination_operation, signed_at)
            .unwrap();
        let issued = destination_store
            .record_issued(
                destination_operation,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    destination_delegate,
                    B256::repeat_byte(3),
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"setup"),
                        signed_at,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();

        // The chain then shows the swap's own account as used, and a later session's job holds
        // no destination owner.
        rpc.state.used.lock().unwrap().insert(used);
        let origin = owner(1, origin_chain);
        let mut accounts =
            SwapAccountReplacements::new(&origin, None, operation, swap_use, |_, _| {});
        if at_submission {
            // A submission retires the used account with the error a preparation gives.
            let found = origin
                .resume_swap_setup(
                    operation,
                    candidate.clone(),
                    &authorization,
                    crate::OperationHttpClient::direct_for_tests(),
                )
                .await
                .err()
                .unwrap();
            let sent = accounts
                .resubmit_with(
                    SwapUseSide::Source,
                    &mut side,
                    Some(Err(found)),
                    &authorization,
                    crate::OperationHttpClient::direct_for_tests(),
                    |_, prepared| async move {
                        let unsent = eyre::eyre!("no broadcaster took the setup");
                        (prepared, Err::<(), _>(unsent))
                    },
                )
                .await;
            assert!(sent.is_some_and(|sent| sent.is_err()));
        } else {
            let resumed = accounts
                .resume_setup(
                    SwapUseSide::Source,
                    &candidate,
                    &authorization,
                    crate::OperationHttpClient::direct_for_tests(),
                )
                .await
                .unwrap();
            side = SwapPairSide::Setup(resumed);
            accounts.save_resumed_approval(saved.clone()).unwrap();
        }
        let replacement = accounts.operation();
        drop(accounts);
        assert_ne!(replacement, operation);
        let SwapPairSide::Setup(setup) = &side else {
            panic!("the replacement needs setup");
        };
        let executor = setup.context().executor;
        assert_ne!(executor, used);

        let bound = origin
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == replacement)
            .and_then(|record| record.swap_use(swap_use)?.approval().cloned())
            .unwrap();
        let accounts = bound.accounts.unwrap();
        assert_eq!(accounts.source.address, Some(executor));
        let after = destination_store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == destination_operation)
            .unwrap();
        assert!(accounts.admits((executor, true), Some((after.address().unwrap(), true))));
        // Nothing else of the approval changed, and nothing of the destination's setup.
        assert_eq!(accounts.destination, approved.destination);
        assert_eq!(
            SwapApproval {
                accounts: saved.accounts,
                ..bound
            },
            saved
        );
        assert_eq!(after.issued(), issued.issued());
        origin.shutdown().await;
        drop(origin);
        drop(destination_store);
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// A setup job replaces at most three used accounts, and none for a hardware authorization,
/// which the device gave for the first account's action only. Either way the job ends with
/// the typed error at the account it last retired, which keeps the swap use, and no account
/// is reserved after it.
#[tokio::test]
async fn replacing_used_accounts_stops_at_the_limit_and_for_a_hardware_authorization() {
    for hardware in [false, true] {
        let rpc = Rpc::start().await;
        rpc.state.all_used.store(true, Ordering::Relaxed);
        let (root, db, vault) = desktop_store_with_vault();
        let descriptor = test_hardware_descriptor(1);
        let view = if hardware {
            crate::vault::tests::executors::spare::hardware::hardware_view(&vault, &descriptor)
        } else {
            Arc::new(import_wallet_with_metadata(
                &vault,
                TEST_WALLET_ID,
                "Wallet",
            ))
        };
        let chain = chain(&rpc);
        let candidate = broadcaster(chain.accepted_executor_profile().unwrap().delegate());
        let owner = Arc::new(
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                chain,
                HttpContext::direct_for_tests(),
            )
            .unwrap(),
        );
        let operation = ExecutorOperationId::random().unwrap();
        let swap_use = SwapUseId::first(operation);
        let authorization = if hardware {
            crate::vault::tests::executors::spare::hardware::authorize(
                &owner,
                &view,
                &descriptor,
                HardwareExecutorAction::Execute(operation),
            )
        } else {
            password()
        };
        let mut replacements = 0;
        let mut accounts =
            SwapAccountReplacements::new(&owner, None, operation, swap_use, |_, _| {
                replacements += 1;
            });
        let prepared = owner
            .prepare_swap_setup(
                operation,
                candidate.clone(),
                setup_approval(WETH, USDC, SwapDelivery::Reshield),
                None,
                &authorization,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await;
        let Err(error) = accounts
            .prepare_setup(
                SwapUseSide::Source,
                prepared,
                &candidate,
                &authorization,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
        else {
            panic!("every account is used");
        };
        let last = accounts.operation();
        drop(accounts);
        assert_eq!(
            replacements,
            if hardware {
                0
            } else {
                SWAP_ACCOUNT_REPLACEMENT_LIMIT
            }
        );
        assert_eq!(last == operation, hardware);
        assert_eq!(
            error
                .downcast_ref::<ExecutorAccountUsed>()
                .map(ExecutorAccountUsed::operation),
            Some(last)
        );
        let records = owner.records().unwrap();
        assert_eq!(
            records.len(),
            replacements + 1,
            "no account is reserved after the last one is retired"
        );
        for record in &records {
            assert!(record.is_retired());
            assert_eq!(
                record.active_swap_use(),
                (record.operation() == last).then_some(swap_use)
            );
        }
        owner.shutdown().await;
        drop(owner);
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}
