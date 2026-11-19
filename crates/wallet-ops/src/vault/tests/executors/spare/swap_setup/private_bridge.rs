use super::*;

#[tokio::test]
async fn private_bridge_setup_claims_both_accounts_first_and_each_setup_stands_alone() {
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
    let mut destination_candidate = broadcaster(destination_profile.delegate());
    destination_candidate.chain_id = DESTINATION_CHAIN;
    let owners = |generation| {
        [origin_chain.clone(), destination_chain.clone()].map(|chain| {
            ExecutorOwner::new(
                generation,
                db.clone(),
                view.clone(),
                chain,
                HttpContext::direct_for_tests(),
            )
            .unwrap()
        })
    };
    let [origin, destination] = owners(0);
    let record = |owner: &ExecutorOwner, operation| {
        owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };
    let (operation, destination_operation) = (
        ExecutorOperationId::random().unwrap(),
        ExecutorOperationId::random().unwrap(),
    );
    // One software authorization covers both chains.
    let authorization = password();
    let destination_authorization = authorization.for_destination().unwrap();
    let preparation = |candidate, approval| SwapPairPreparation {
        use_id: SwapUseId::first(operation),
        source: SwapAccountChoice::New(operation),
        destination: Some(SwapAccountChoice::New(destination_operation)),
        candidate: Some(candidate),
        destination_candidate: Some(destination_candidate.clone()),
        approval,
        authorization: &authorization,
        destination_authorization: Some(&destination_authorization),
        client: crate::OperationHttpClient::direct_for_tests(),
    };

    // Nothing is reserved for terms that are not a private Bridge delivery with a destination
    // setup fee limit, or for a swap whose link disagrees with its delivery.
    let mut unbounded = private_bridge_approval();
    unbounded.bounds.destination_setup_fee = None;
    for refused in [
        setup_approval(WETH, USDC, SwapDelivery::Reshield),
        unbounded,
    ] {
        let request = preparation(broadcaster(profile.delegate()), refused);
        assert!(
            prepare_swap_pair(&origin, Some(&destination), request)
                .await
                .is_err()
        );
    }
    for (approval, link) in [
        (private_bridge_approval(), None),
        (
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            Some(destination_operation),
        ),
    ] {
        let candidate = broadcaster(profile.delegate());
        assert!(
            origin
                .prepare_swap_setup(
                    operation,
                    candidate,
                    approval,
                    link,
                    &authorization,
                    crate::OperationHttpClient::direct_for_tests()
                )
                .await
                .is_err()
        );
    }
    assert!(origin.records().unwrap().is_empty() && destination.records().unwrap().is_empty());

    // Both accounts are claimed for the swap's use before either is inspected, so a preparation
    // paused in a chain read holds its whole pair, and a setup that fails leaves both claimed.
    let swap_use = SwapUseId::first(operation);
    // What an unrelated swap gets when it names either account of the pair.
    let unrelated_claims = |origin: &ExecutorOwner, destination: &ExecutorOwner| {
        [
            origin.claim_swap_use(
                None,
                SwapUseClaim {
                    id: SwapUseId::random().unwrap(),
                    source: SwapAccountChoice::Existing(operation),
                    approval: setup_approval(WETH, USDC, SwapDelivery::Reshield),
                    destination: None,
                },
            ),
            origin.claim_swap_use(
                Some(destination),
                SwapUseClaim {
                    id: SwapUseId::random().unwrap(),
                    source: SwapAccountChoice::New(ExecutorOperationId::random().unwrap()),
                    approval: private_bridge_approval(),
                    destination: Some(SwapAccountChoice::Existing(destination_operation)),
                },
            ),
        ]
        .map(|claim| {
            matches!(
                claim.unwrap_err().downcast_ref::<ExecutorStoreError>(),
                Some(ExecutorStoreError::SwapUseActive)
            )
        })
    };
    // The pair as its records hold it.
    let pair = |origin: &ExecutorOwner, destination: &ExecutorOwner| {
        let (source, account) = (
            record(origin, operation),
            record(destination, destination_operation),
        );
        (
            (source.active_swap_use(), account.active_swap_use()),
            source.swap_approval().cloned(),
            source.destination_operation(),
            account.swap_destination(),
            (source.is_retired(), account.is_retired()),
        )
    };
    // The swap's own setup fails at admission: its broadcaster lists no accepted delegate.
    let unsupported = broadcaster(Address::repeat_byte(0x99));
    let request = preparation(unsupported, private_bridge_approval());
    rpc.hold.send_replace(Some(multicall()));
    {
        let preparing = prepare_swap_pair(&origin, Some(&destination), request);
        tokio::pin!(preparing);
        tokio::select! {
            result = &mut preparing => panic!("destination inspection should be blocked: {}", result.is_ok()),
            () = rpc.wait_for(|requests| requests.iter().any(|request| request["method"] == "eth_call")) => {},
        }
        let held = (
            record(&origin, operation),
            record(&destination, destination_operation),
        );
        assert_eq!(
            (held.0.active_swap_use(), held.1.active_swap_use()),
            (Some(swap_use), Some(swap_use))
        );
        assert_eq!(held.0.destination_operation(), Some(destination_operation));
        assert!(is_swap_destination_record(&held.1));
        // An unrelated swap takes neither account, and its own new account is not allocated.
        assert_eq!(unrelated_claims(&origin, &destination), [true, true]);
        // Finishing an older swap sweeps this chain while the pair is still being inspected,
        // and so does a restart's load. Neither retires or releases an account of the pair.
        assert!(!destination.reconcile_swap_destinations().unwrap());
        assert!(
            !ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN)
                .unwrap()
                .reconcile_swap_destinations_on_load()
                .unwrap()
        );
        assert_eq!(
            (origin.records().unwrap(), destination.records().unwrap()),
            (vec![held.0], vec![held.1])
        );
        rpc.hold.send_replace(None);
        assert!(preparing.await.is_err());
    }
    let reserved = record(&destination, destination_operation);
    assert!(is_swap_destination_record(&reserved) && !crate::is_swap_record(&reserved));
    assert!(crate::is_swap_record(&record(&origin, operation)));

    // A restart before anything is signed restores the pair with its approval and its claim,
    // and still gives neither account to another swap.
    let claimed = pair(&origin, &destination);
    assert_eq!(claimed.0, (Some(swap_use), Some(swap_use)));
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
    let [origin, destination] = owners(1);
    assert_eq!(pair(&origin, &destination), claimed);
    assert_eq!(unrelated_claims(&origin, &destination), [true, true]);
    assert!(record(&origin, operation).issued().is_empty() && reserved.issued().is_empty());

    // The preparation stopped with only the destination account derived, so the saved approval
    // still holds the placeholder receiver. Nothing is bound or written while the swap's own
    // account has no address.
    assert!(record(&origin, operation).address().is_none());
    assert!(
        origin
            .bind_swap_use_accounts(operation, swap_use, None)
            .is_err()
    );
    assert_eq!(pair(&origin, &destination), claimed);
    // The retry of the swap's own setup derives that account. The approval then binds both
    // accounts and the delivery's receiver, and takes a fee limit reviewed since. No other term
    // changes.
    let resumed = origin
        .resume_swap_setup(
            operation,
            broadcaster(profile.delegate()),
            &authorization,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    let mut raised = private_bridge_approval();
    raised.bounds.source_setup_fee = Some(setup_fee_limit(3_000));
    let bound = origin
        .bind_swap_use_accounts(operation, swap_use, Some(raised.clone()))
        .unwrap();
    raised.accounts = Some(SwapApprovedAccounts {
        source: SwapApprovedAccount {
            address: Some(resumed.context().executor),
            setup: true,
        },
        destination: Some(SwapApprovedAccount {
            address: reserved.address(),
            setup: true,
        }),
    });
    let SwapDelivery::Bridge(delivery) = &mut raised.delivery else {
        panic!("the approval keeps its Bridge delivery");
    };
    delivery.receiver = reserved.address().unwrap();
    assert_eq!(bound, raised);
    assert_eq!(record(&origin, operation).swap_approval(), Some(&bound));

    // The retry resumes both accounts, and the approval saved with the swap names the
    // destination account.
    let request = preparation(broadcaster(profile.delegate()), private_bridge_approval());
    let prepared = prepare_swap_pair(&origin, Some(&destination), request)
        .await
        .unwrap();
    let (SwapPairSide::Setup(origin_setup), Some(SwapPairSide::Setup(destination_setup))) =
        (&prepared.origin, &prepared.destination)
    else {
        panic!("both fresh accounts need setup");
    };
    let executor = destination_setup.context().executor;
    assert_eq!(reserved.address(), Some(executor));
    assert_ne!(origin_setup.context().executor, executor);
    let SwapDelivery::Bridge(delivery) = prepared.approval.delivery else {
        panic!("the approval keeps its Bridge delivery");
    };
    assert_eq!(delivery.receiver, executor);
    let saved = record(&origin, operation);
    assert_eq!(saved.swap_approval(), Some(&prepared.approval));
    assert_eq!(saved.destination_operation(), Some(destination_operation));
    let serves = SwapDestinationRecord {
        origin_chain: 1,
        origin_operation: operation,
        destination_token: DESTINATION_TOKEN,
        outcome: None,
    };
    assert_eq!(
        record(&destination, destination_operation).swap_destination(),
        Some(serves)
    );

    // A duplicate Public address rejects the handoff without stopping either fresh account.
    let duplicate = {
        let (_, signer) = vault
            .executor_spend_signers_for_session(
                &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
                &view,
                None,
                1,
                saved.index(),
            )
            .unwrap();
        let private_key = Zeroizing::new(alloy::hex::encode(signer.to_bytes()));
        vault
            .import_public_account(TEST_PASSWORD, &view, &private_key, None, false)
            .unwrap()
    };
    assert_eq!(duplicate.address, origin_setup.context().executor);
    let before = (
        origin.records().unwrap(),
        destination.records().unwrap(),
        vault.list_public_accounts_for_session(&view, true).unwrap(),
    );
    let error = origin
        .register_public_account(operation, &authorization)
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ExecutorStoreError>(),
        Some(ExecutorStoreError::Vault(
            VaultError::DuplicatePublicAccountAddress
        ))
    ));
    assert_eq!(
        (
            origin.records().unwrap(),
            destination.records().unwrap(),
            vault.list_public_accounts_for_session(&view, true).unwrap(),
        ),
        before
    );
    vault
        .delete_imported_public_account(&view, &duplicate.public_account_uuid)
        .unwrap();

    // The destination setup's fee ceiling is the one approved with the swap, read from the
    // swap's record on its own chain.
    let fee_token = destination_candidate.token;
    destination
        .require_swap_destination_setup_fee(destination_operation, fee_token, U256::from(1_000))
        .unwrap();
    let error = destination
        .require_swap_destination_setup_fee(destination_operation, fee_token, U256::from(1_001))
        .unwrap_err();
    let exceeded = error
        .downcast_ref::<ExecutorPrivateFeeLimitExceeded>()
        .unwrap();
    assert_eq!(
        (exceeded.maximum(), exceeded.required()),
        (U256::from(1_000), U256::from(1_001))
    );
    // Another fee token is refused even for fewer base units.
    assert!(
        destination
            .require_swap_destination_setup_fee(
                destination_operation,
                Address::repeat_byte(0x34),
                U256::from(999)
            )
            .unwrap_err()
            .downcast_ref::<ExecutorPrivateFeeTokenChanged>()
            .is_some()
    );
    assert!(
        origin
            .require_swap_destination_setup_fee(operation, fee_token, U256::ZERO)
            .is_err()
    );

    // The swap's setup nonce is read as consumed and the destination's as unconsumed.
    let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
    let settle = |chain_id: u64,
                  operation: ExecutorOperationId,
                  delegate: Address,
                  consumed: bool| {
        let store = ExecutorStore::new(db.clone(), view.clone(), chain_id).unwrap();
        let signed_at =
            ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
        store.record_account_read(operation, signed_at).unwrap();
        let setup = B256::repeat_byte(3);
        store
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    delegate,
                    setup,
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"setup"),
                        signed_at,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        store
            .record_account_read(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::from(u8::from(consumed))),
            )
            .unwrap()
    };
    let status = |record: &ExecutorRecord, profile: crate::settings::ExecutorProfile| {
        let code = [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            profile.delegate().as_slice(),
        ]
        .concat();
        swap_setup_status(record, confirmed, &code, profile)
    };
    let delegated = settle(1, operation, profile.delegate(), true);
    let pending = settle(
        DESTINATION_CHAIN,
        destination_operation,
        destination_profile.delegate(),
        false,
    );
    assert!(matches!(
        status(&delegated, profile),
        SwapSetupStatus::Delegated(_)
    ));
    assert_eq!(
        status(&pending, destination_profile),
        SwapSetupStatus::Pending
    );

    // A retry prepares only the pending setup, with the account it reserved. The confirmed
    // setup takes no retry and its record is untouched.
    let retried = destination
        .resume_swap_setup(
            destination_operation,
            destination_candidate.clone(),
            &destination_authorization,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(retried.context().executor, executor);
    assert!(
        origin
            .resume_swap_setup(
                operation,
                broadcaster(profile.delegate()),
                &authorization,
                crate::OperationHttpClient::direct_for_tests()
            )
            .await
            .is_err()
    );
    assert_eq!(record(&origin, operation), delegated);

    // A restart restores both accounts and the saved approval, with the destination setup
    // still to retry for the same swap and token only.
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
    let [origin, destination] = owners(2);
    assert_eq!(
        record(&origin, operation).swap_approval(),
        Some(&prepared.approval)
    );
    let restored = record(&destination, destination_operation);
    assert!(!restored.is_retired());
    assert_eq!(restored.swap_destination(), Some(serves));
    let resumed = destination
        .resume_swap_setup(
            destination_operation,
            destination_candidate.clone(),
            &destination_authorization,
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    assert_eq!(resumed.context().executor, executor);
    let before = (origin.records().unwrap(), destination.records().unwrap());
    let mut another = prepared.approval.clone();
    let SwapDelivery::Bridge(delivery) = &mut another.delivery else {
        panic!("the approval keeps its Bridge delivery");
    };
    delivery.destination_token = USDC;
    let error = prepare_swap_pair(
        &origin,
        Some(&destination),
        preparation(broadcaster(profile.delegate()), another),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(
        error.downcast_ref::<ExecutorStoreError>(),
        Some(ExecutorStoreError::OperationMismatch)
    ));
    assert_eq!(
        (origin.records().unwrap(), destination.records().unwrap()),
        before
    );
    // Registering the swap's account in Public stops its use before the account is handed off,
    // and the stopped destination account takes no further setup.
    let live = record(&origin, operation);
    assert!(!live.swap_use(swap_use).unwrap().is_stopped());
    origin
        .register_public_account(operation, &authorization)
        .await
        .unwrap();
    let registered = record(&origin, operation);
    assert!(registered.swap_use(swap_use).unwrap().is_stopped());
    assert_eq!(registered.issued(), live.issued());
    assert!(
        destination
            .resume_swap_setup(
                destination_operation,
                destination_candidate,
                &destination_authorization,
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
            .is_err()
    );
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
