use super::*;

// Each side of a pair is a new account, which the swap sets up, or an existing one, which takes
// no setup and no setup fee. Only new sides are prepared and submitted, each with its own
// chain's broadcaster and approved fee limit, and two new sides are submitted at the same time.
#[tokio::test]
async fn swap_pair_sets_up_only_its_new_accounts() {
    for (source_new, destination_new) in
        [(true, true), (false, true), (true, false), (false, false)]
    {
        let rpc = Rpc::start().await;
        let (root, db, vault) = desktop_store_with_vault();
        let view = Arc::new(import_wallet_with_metadata(
            &vault,
            TEST_WALLET_ID,
            "Wallet",
        ));
        let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
        let profile = origin_chain.accepted_executor_profile().unwrap();
        let destination_profile = destination_chain.accepted_executor_profile().unwrap();
        let candidate = broadcaster(profile.delegate());
        let mut destination_candidate = broadcaster(destination_profile.delegate());
        destination_candidate.chain_id = DESTINATION_CHAIN;
        let [origin, destination] = [origin_chain, destination_chain].map(|chain| {
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                chain,
                HttpContext::direct_for_tests(),
            )
            .unwrap()
        });
        let record = |owner: &ExecutorOwner, operation| {
            owner
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap()
        };
        let existing = (Address::repeat_byte(0xa1), Address::repeat_byte(0xa2));
        let source = if source_new {
            SwapAccountChoice::New(ExecutorOperationId::random().unwrap())
        } else {
            let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
            SwapAccountChoice::Existing(existing_account(&store, profile, existing.0))
        };
        let destination_account = if destination_new {
            SwapAccountChoice::New(ExecutorOperationId::random().unwrap())
        } else {
            let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
            SwapAccountChoice::Existing(existing_account(&store, destination_profile, existing.1))
        };
        let other_destination = if !source_new && !destination_new {
            let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
            let address = Address::repeat_byte(0xa3);
            existing_account(&store, destination_profile, address);
            Some(address)
        } else {
            None
        };
        let before = (origin.records().unwrap(), destination.records().unwrap());
        let use_id = SwapUseId::random().unwrap();
        let authorization = password();
        let destination_authorization = authorization.for_destination().unwrap();
        // The approval binds a fee limit only for a chain whose account needs setup.
        let mut approval = private_bridge_approval();
        approval.bounds.source_setup_fee = source_new.then_some(setup_fee_limit(2_000));
        approval.bounds.destination_setup_fee = destination_new.then_some(setup_fee_limit(1_000));
        let preparation =
            |source, approval, candidate, destination_candidate| SwapPairPreparation {
                use_id,
                source,
                destination: Some(destination_account),
                approval,
                candidate,
                destination_candidate,
                authorization: &authorization,
                destination_authorization: Some(&destination_authorization),
                client: crate::OperationHttpClient::direct_for_tests(),
            };
        // The broadcaster of each side that needs setup.
        let source_setup = source_new.then(|| candidate.clone());
        let destination_setup = destination_new.then(|| destination_candidate.clone());

        // A broadcaster on a side that isn't new, a new side without one, and a new side
        // without its fee limit each reserve nothing.
        let mut unbounded = approval.clone();
        unbounded.bounds.source_setup_fee = None;
        unbounded.bounds.destination_setup_fee = None;
        let mut refused = vec![
            (
                approval.clone(),
                (!source_new).then(|| candidate.clone()),
                destination_setup.clone(),
            ),
            (
                approval.clone(),
                source_setup.clone(),
                (!destination_new).then(|| destination_candidate.clone()),
            ),
        ];
        if source_new || destination_new {
            refused.push((unbounded, source_setup.clone(), destination_setup.clone()));
        }
        if let Some(other_destination) = other_destination {
            // Software authorization cannot substitute the selected destination for the
            // account explicitly approved, or change whether either side needs setup.
            let mut bound = approval.clone();
            bound.accounts = Some(SwapApprovedAccounts {
                source: SwapApprovedAccount {
                    address: Some(existing.0),
                    setup: false,
                },
                destination: Some(SwapApprovedAccount {
                    address: Some(other_destination),
                    setup: false,
                }),
            });
            refused.push((bound.clone(), None, None));
            let accounts = bound.accounts.as_mut().unwrap();
            accounts.destination.as_mut().unwrap().address = Some(existing.1);
            accounts.source.setup = true;
            refused.push((bound.clone(), None, None));
            let accounts = bound.accounts.as_mut().unwrap();
            accounts.source.setup = false;
            accounts.destination = None;
            refused.push((bound, None, None));
        }
        for (approval, candidate, destination_candidate) in refused {
            let request = preparation(source, approval, candidate, destination_candidate);
            assert!(
                prepare_swap_pair(&origin, Some(&destination), request)
                    .await
                    .is_err()
            );
            assert_eq!(
                (origin.records().unwrap(), destination.records().unwrap()),
                before
            );
        }

        let request = preparation(
            source,
            approval.clone(),
            source_setup.clone(),
            destination_setup.clone(),
        );
        let prepared = prepare_swap_pair(&origin, Some(&destination), request)
            .await
            .unwrap();
        let prepared_destination = prepared.destination.as_ref().unwrap();
        assert_eq!(
            (
                prepared.origin.requires_setup(),
                prepared_destination.requires_setup()
            ),
            (source_new, destination_new)
        );

        // A new side is prepared for its own chain and its own broadcaster. An existing side
        // keeps its address and what it issued, and its use is not a fresh one.
        for (owner, side, chain_id, own, other, address) in [
            (
                &origin,
                &prepared.origin,
                1,
                &candidate,
                &destination_candidate,
                existing.0,
            ),
            (
                &destination,
                prepared_destination,
                DESTINATION_CHAIN,
                &destination_candidate,
                &candidate,
                existing.1,
            ),
        ] {
            let saved = record(owner, side.operation());
            assert_eq!(saved.active_swap_use(), Some(use_id));
            assert_eq!(saved.address(), Some(side.executor()));
            match side {
                SwapPairSide::Setup(setup) => {
                    assert_eq!(setup.context().chain_id, chain_id);
                    setup.require_broadcaster(own).unwrap();
                    assert!(setup.require_broadcaster(other).is_err());
                    assert!(saved.swap_use(use_id).unwrap().is_fresh());
                    assert_eq!(saved.issued(), [] as [IssuedExecutorPayload; 0]);
                }
                SwapPairSide::Existing { executor, .. } => {
                    assert_eq!(*executor, address);
                    assert!(!saved.swap_use(use_id).unwrap().is_fresh());
                    assert_eq!(saved.issued().len(), 1, "only its earlier setup");
                }
            }
        }

        // The saved approval binds both addresses, each side's setup need and the destination
        // account as receiver.
        let bound = crate::vault::SwapApprovedAccounts {
            source: crate::vault::SwapApprovedAccount {
                address: Some(prepared.origin.executor()),
                setup: source_new,
            },
            destination: Some(crate::vault::SwapApprovedAccount {
                address: Some(prepared_destination.executor()),
                setup: destination_new,
            }),
        };
        assert_eq!(prepared.approval.accounts, Some(bound));
        let SwapDelivery::Bridge(delivery) = prepared.approval.delivery else {
            panic!("the approval keeps its Bridge delivery");
        };
        assert_eq!(delivery.receiver, prepared_destination.executor());
        let saved = record(&origin, source.operation());
        assert_eq!(saved.swap_approval(), Some(&prepared.approval));
        if let Some(other_destination) = other_destination {
            // An idempotent claim must still enforce the supplied account binding.
            let before = (origin.records().unwrap(), destination.records().unwrap());
            let mut replaced = prepared.approval.clone();
            replaced
                .accounts
                .as_mut()
                .unwrap()
                .destination
                .as_mut()
                .unwrap()
                .address = Some(other_destination);
            let request = preparation(source, replaced, None, None);
            assert!(
                prepare_swap_pair(&origin, Some(&destination), request)
                    .await
                    .is_err()
            );
            assert_eq!(
                (origin.records().unwrap(), destination.records().unwrap()),
                before
            );
        }

        // Each new side's fee limit is the one approved for its own chain.
        let exceeds = |result: eyre::Result<()>| {
            let error = result.unwrap_err();
            let exceeded = error
                .downcast_ref::<ExecutorPrivateFeeLimitExceeded>()
                .unwrap();
            (exceeded.maximum(), exceeded.required())
        };
        // Another fee token is refused even for fewer base units than the limit.
        let other_token = Address::repeat_byte(0x34);
        let changed = |result: eyre::Result<()>| {
            let error = result.unwrap_err();
            let changed = error
                .downcast_ref::<ExecutorPrivateFeeTokenChanged>()
                .unwrap();
            (
                changed.approved_token(),
                changed.approved_amount(),
                changed.fee_token(),
                changed.required(),
            )
        };
        if source_new {
            let (operation, token) = (source.operation(), candidate.token);
            origin
                .require_swap_source_setup_fee(operation, token, U256::from(2_000))
                .unwrap();
            assert_eq!(
                exceeds(origin.require_swap_source_setup_fee(operation, token, U256::from(2_001))),
                (U256::from(2_000), U256::from(2_001))
            );
            assert_eq!(
                changed(origin.require_swap_source_setup_fee(
                    operation,
                    other_token,
                    U256::from(1_999)
                )),
                (token, U256::from(2_000), other_token, U256::from(1_999))
            );
        }
        if destination_new {
            let (operation, token) = (destination_account.operation(), destination_candidate.token);
            destination
                .require_swap_destination_setup_fee(operation, token, U256::from(1_000))
                .unwrap();
            assert_eq!(
                exceeds(destination.require_swap_destination_setup_fee(
                    operation,
                    token,
                    U256::from(1_001)
                )),
                (U256::from(1_000), U256::from(1_001))
            );
            assert_eq!(
                changed(destination.require_swap_destination_setup_fee(
                    operation,
                    other_token,
                    U256::from(999)
                )),
                (token, U256::from(1_000), other_token, U256::from(999))
            );
        }

        // Only new sides are submitted. Each submission waits for the others, so two new sides
        // finish only when both are in flight at once.
        let new_sides = usize::from(source_new) + usize::from(destination_new);
        let barrier = tokio::sync::Barrier::new(new_sides.max(1));
        let submitted = Mutex::new(Vec::new());
        let results = tokio::time::timeout(
            Duration::from_secs(2),
            submit_swap_pair_setups_with(
                (&origin, &prepared.origin, Some(U256::from(2_000))),
                Some((&destination, prepared_destination, Some(U256::from(1_000)))),
                |_, setup, maximum_private_fee| {
                    let (barrier, submitted) = (&barrier, &submitted);
                    async move {
                        barrier.wait().await;
                        let sent = (setup.context().chain_id, setup.operation());
                        submitted.lock().unwrap().push((sent, maximum_private_fee));
                        Ok::<_, eyre::Report>(sent)
                    }
                },
            ),
        )
        .await
        .expect("new sides are submitted at the same time");
        let sent_origin = source_new.then(|| ((1, source.operation()), U256::from(2_000)));
        let sent_destination = destination_new.then(|| {
            (
                (DESTINATION_CHAIN, destination_account.operation()),
                U256::from(1_000),
            )
        });
        assert_eq!(
            (results.0.map(Result::unwrap), results.1.map(Result::unwrap)),
            (
                sent_origin.map(|(sent, _)| sent),
                sent_destination.map(|(sent, _)| sent)
            )
        );
        let mut submitted = submitted.into_inner().unwrap();
        submitted.sort_by_key(|((chain_id, _), _)| *chain_id);
        assert_eq!(
            submitted,
            sent_origin
                .into_iter()
                .chain(sent_destination)
                .collect::<Vec<_>>()
        );

        // A retry prepares a new side alone, with the account it reserved. An existing side
        // takes no setup, and its record stays as it is.
        for (owner, side, candidate, authorization) in [
            (&origin, &prepared.origin, &candidate, &authorization),
            (
                &destination,
                prepared_destination,
                &destination_candidate,
                &destination_authorization,
            ),
        ] {
            let saved = record(owner, side.operation());
            let retried = owner
                .resume_swap_setup(
                    side.operation(),
                    candidate.clone(),
                    authorization,
                    crate::OperationHttpClient::direct_for_tests(),
                )
                .await;
            if side.requires_setup() {
                assert_eq!(retried.unwrap().context().executor, side.executor());
            } else {
                let error = retried.err().unwrap();
                assert!(error.to_string().contains("takes no setup"), "{error:#}");
                assert_eq!(record(owner, side.operation()), saved);
            }
        }

        // The same request resumes the pair. One that flips the source's setup need for the
        // same use is refused, and the saved approval stays.
        let request = preparation(
            source,
            prepared.approval.clone(),
            source_setup,
            destination_setup.clone(),
        );
        let resumed = prepare_swap_pair(&origin, Some(&destination), request)
            .await
            .unwrap();
        assert_eq!(resumed.approval, prepared.approval);
        let flipped = if source_new {
            SwapAccountChoice::Existing(source.operation())
        } else {
            SwapAccountChoice::New(source.operation())
        };
        let mut rebound = approval;
        rebound.bounds.source_setup_fee = Some(setup_fee_limit(2_000));
        let request = preparation(
            flipped,
            rebound,
            (!source_new).then(|| candidate.clone()),
            destination_setup,
        );
        let error = prepare_swap_pair(&origin, Some(&destination), request)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("differ"), "{error:#}");
        assert_eq!(
            record(&origin, source.operation()).swap_approval(),
            Some(&prepared.approval)
        );

        origin.shutdown().await;
        destination.shutdown().await;
        drop((origin, destination));
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn a_swap_use_cancelled_during_inspection_refuses_its_late_preparation() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
    let profile = origin_chain.accepted_executor_profile().unwrap();
    let mut destination_candidate = broadcaster(
        destination_chain
            .accepted_executor_profile()
            .unwrap()
            .delegate(),
    );
    destination_candidate.chain_id = DESTINATION_CHAIN;
    let [origin, destination] = [origin_chain, destination_chain].map(|chain| {
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain,
            HttpContext::direct_for_tests(),
        )
        .unwrap()
    });
    let (operation, destination_operation) = (
        ExecutorOperationId::random().unwrap(),
        ExecutorOperationId::random().unwrap(),
    );
    let authorization = password();
    let destination_authorization = authorization.for_destination().unwrap();
    let preparation = || SwapPairPreparation {
        use_id: SwapUseId::first(operation),
        source: SwapAccountChoice::New(operation),
        destination: Some(SwapAccountChoice::New(destination_operation)),
        candidate: Some(broadcaster(profile.delegate())),
        destination_candidate: Some(destination_candidate.clone()),
        approval: private_bridge_approval(),
        authorization: &authorization,
        destination_authorization: Some(&destination_authorization),
        client: crate::OperationHttpClient::direct_for_tests(),
    };

    // The user cancels while the destination account is still being inspected. Neither fresh
    // account issued anything, so both claims are released, and the inspection's result
    // prepares nothing.
    rpc.hold.send_replace(Some(multicall()));
    {
        let preparing = prepare_swap_pair(&origin, Some(&destination), preparation());
        tokio::pin!(preparing);
        tokio::select! {
            result = &mut preparing => panic!("destination inspection should be blocked: {}", result.is_ok()),
            () = rpc.wait_for(|requests| requests.iter().any(|request| request["method"] == "eth_call")) => {},
        }
        assert_eq!(
            origin
                .cancel_swap_use(Some(&destination), operation, SwapUseId::first(operation))
                .unwrap(),
            SwapUseCancellation {
                source: SwapUseRelease::Released,
                destination: Some(SwapUseRelease::Released),
            }
        );
        rpc.hold.send_replace(None);
        assert!(preparing.await.is_err());
    }
    // Starting the same swap again resumes its stopped claim, which prepares nothing either.
    assert!(
        prepare_swap_pair(&origin, Some(&destination), preparation())
            .await
            .is_err()
    );
    for (owner, operation) in [(&origin, operation), (&destination, destination_operation)] {
        let records = owner.records().unwrap();
        let [record] = records.as_slice() else {
            panic!("the cancelled swap keeps its one account on each chain");
        };
        assert_eq!(record.operation(), operation);
        assert!(record.is_swap_setup_stopped() && record.issued().is_empty());
        assert!(record.swap_uses().iter().all(SwapUseRecord::is_stopped));
    }
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// Each pass of a setup wait is one read of the account's code and execution nonce at the
// confirmed tip, and that state alone decides the setup.
#[tokio::test]
async fn swap_setup_completes_only_with_observed_delegation_and_consumed_nonce() {
    use railgun_wallet::WalletUtxo;
    use sync_service::{WalletCurrentSnapshot, WalletPendingOverlay};

    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let profile = chain.accepted_executor_profile().unwrap();
    let delegate = profile.delegate();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let prepared = owner
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
    let executor = prepared.context().executor;
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
            _nonce: prepared.context().execution_nonce,
            _signature: Bytes::new(),
        }
        .abi_encode()
        .into(),
    };
    let setup = owner
        .issue_operation(
            &prepared,
            &call,
            std::slice::from_ref(&input),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap()
        .payload_hash();
    store
        .record_submission(operation, setup, B256::repeat_byte(5))
        .unwrap();
    let record = || {
        owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };
    // One pass at the confirmed block below `head`, with the requests it made.
    let pass = async |head: u64| {
        rpc.state.head.store(head, Ordering::Relaxed);
        let before = rpc.state.requests.lock().unwrap().len();
        let status = owner
            .observe_swap_setup(operation, crate::OperationHttpClient::direct_for_tests())
            .await
            .unwrap()
            .status;
        let requests = rpc.state.requests.lock().unwrap()[before..].to_vec();
        (status, requests)
    };
    let reads_account = |requests: &[Value]| {
        requests.iter().any(|request| {
            request["method"] == "eth_getCode" && request["params"][0] == json!(executor)
        })
    };

    // The nonce is unconsumed at the confirmed block: not yet included, or included with
    // its authorization skipped. The setup stays pending and the next pass reads again,
    // without whole blocks or logs.
    for head in [20, 21] {
        let (status, requests) = pass(head).await;
        assert_eq!(status, SwapSetupStatus::Pending);
        assert!(reads_account(&requests));
        assert!(requests.iter().all(|request| {
            request["method"] != "eth_getLogs"
                && (request["method"] != "eth_getBlockByNumber"
                    || request["params"][1] != json!(true))
        }));
        assert_eq!(
            record().nonce_observation().unwrap().block().number,
            head - 1
        );
    }

    // Another writer changes the record while the pass reads. The pass keeps waiting.
    rpc.hold.send_replace(Some(executor));
    let held = rpc.state.requests.lock().unwrap().len();
    let (status, ()) = tokio::join!(
        owner.observe_swap_setup(operation, crate::OperationHttpClient::direct_for_tests()),
        async {
            rpc.wait_for(|requests| reads_account(&requests[held..]))
                .await;
            owner.set_hidden(operation, true).unwrap();
            rpc.hold.send_replace(None);
        }
    );
    assert_eq!(status.unwrap().status, SwapSetupStatus::Pending);
    owner.set_hidden(operation, false).unwrap();

    // The user releases the setup's notes and a transaction that is not recorded for the
    // setup spends them. That is no evidence about the setup: it stays pending and a retry
    // is admitted with the same account.
    owner.release_input_lock(operation).unwrap();
    let mut spent = WalletUtxo::new(input);
    spent.spent = Some(UtxoSource {
        tx_hash: B256::repeat_byte(6),
        block_number: 14,
        block_timestamp: 1,
    });
    let synced = WalletCurrentSnapshot::new(21, 0, 0, vec![spent], WalletPendingOverlay::default());
    owner
        .confirm_synced_record(&synced, &|| Some(synced.clone()), Some(21), record())
        .await
        .unwrap();
    assert_eq!(
        record().payload_state(setup),
        Some(ExecutorPayloadState::Pending)
    );
    assert_eq!(pass(22).await.0, SwapSetupStatus::Pending);
    // A retry through a broadcaster advertising the earlier listed delegate moves the record
    // to it; the account the first setup delegated to the primary is still recognized below.
    let earlier = broadcaster_core::deployment::RailgunDeployment::for_chain(1)
        .unwrap()
        .relay_adapt_7702_delegates[0]
        .address;
    let retried = owner
        .resume_swap_setup(
            operation,
            broadcaster(earlier),
            &password(),
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(retried.context().executor, executor);
    assert_eq!(
        (retried.context().delegate, record().delegate()),
        (earlier, earlier)
    );

    // The nonce is past the setup and the account has no code: the delegation is missing.
    rpc.state.used.lock().unwrap().insert(executor);
    assert_eq!(pass(23).await.0, SwapSetupStatus::MissingDelegation);

    // The nonce is past the setup under the accepted delegation: the account is set up.
    rpc.set_delegated_account(executor, delegate, U256::ONE);
    let (SwapSetupStatus::Delegated(handle), _) = pass(24).await else {
        panic!("the confirmed delegation completes the setup");
    };
    assert_eq!(
        (
            handle.operation(),
            handle.executor(),
            handle.setup_payload()
        ),
        (operation, executor, setup)
    );
    // Code read at any block other than the nonce observation is not evidence.
    let code = [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        delegate.as_slice(),
    ]
    .concat();
    assert_eq!(
        swap_setup_status(
            &record(),
            BlockNumHash::new(22, B256::repeat_byte(22)),
            &code,
            crate::settings::ExecutorProfile::accepted(1, earlier).unwrap()
        ),
        SwapSetupStatus::Pending
    );

    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
