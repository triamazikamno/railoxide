use super::*;
use crate::public_wallet::{WalletConnectPersonalSignRequest, walletconnect_sign_personal_message};
use crate::signer::EvmTransactionSigner as _;
use crate::{DesktopPrivateSpendAuthorization, HardwareExecutorAction};

pub(in crate::vault::tests::executors) fn hardware_view(
    vault: &DesktopVaultStore,
    descriptor: &HardwareDerivationDescriptor,
) -> Arc<DesktopViewSession> {
    let wallet = test_hardware_wallet(descriptor.account_index);
    let metadata = vault
        .new_hardware_wallet_metadata(
            TEST_PASSWORD,
            TEST_WALLET_ID,
            "Hardware",
            descriptor.clone(),
        )
        .unwrap();
    vault
        .store_hardware_derived_wallet_with_metadata(
            TEST_PASSWORD,
            TEST_WALLET_ID,
            descriptor.account_index,
            &wallet,
            &metadata,
            &test_hardware_view_access_key(descriptor.account_index),
        )
        .unwrap();
    Arc::new(load_test_hardware_view_session(
        vault,
        TEST_WALLET_ID,
        descriptor,
    ))
}

pub(super) fn authorize(
    owner: &Arc<ExecutorOwner>,
    view: &Arc<DesktopViewSession>,
    descriptor: &HardwareDerivationDescriptor,
    action: HardwareExecutorAction,
) -> DesktopPrivateSpendAuthorization {
    let needs_root = matches!(
        action,
        HardwareExecutorAction::Execute(_) | HardwareExecutorAction::Restore(_)
    );
    let authorization = owner
        .hardware_authorization_request(view.clone(), action)
        .unwrap()
        .complete(descriptor, &[42; 32])
        .unwrap();
    assert_eq!(
        authorization.retains_root_seed_for_test(),
        needs_root,
        "fixed-account approvals must discard the root before returning to async work"
    );
    DesktopPrivateSpendAuthorization::HardwareExecutor(Box::new(authorization))
}

#[tokio::test]
async fn hardware_executor_allocation_issuance_and_public_signing_require_scoped_derivation() {
    for descriptor in [
        test_hardware_descriptor(1),
        test_trezor_hardware_descriptor(1),
    ] {
        let rpc = Rpc::start().await;
        let (root, db, vault) = desktop_store_with_vault();
        let vault = Arc::new(vault);
        let view = hardware_view(&vault, &descriptor);
        let owner = Arc::new(
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                chain(&rpc),
                HttpContext::direct_for_tests(),
            )
            .unwrap(),
        );
        let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        let payer = vault
            .import_public_account(TEST_PASSWORD, &view, IMPORT_PRIVATE_KEY_ONE, None, true)
            .unwrap();
        let delivery = || ExecutorDelivery::SelfBroadcast {
            sender: payer.address,
            sponsored: false,
        };
        let operation = ExecutorOperationId::random().unwrap();
        let password =
            DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(TEST_PASSWORD.into()));
        assert!(
            owner
                .prepare_operation(operation, delivery(), &password, &[], None)
                .await
                .is_err()
        );
        let private = DesktopPrivateSpendAuthorization::PreauthorizedSigner(
            vault
                .hardware_railgun_spend_signer_from_entropy(&view, &descriptor, &[42; 32])
                .unwrap(),
        );
        assert!(
            owner
                .prepare_operation(operation, delivery(), &private, &[], None)
                .await
                .is_err()
        );
        let native_descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
            descriptor.device_kind,
            descriptor.account_index,
            0,
        )
        .unwrap();
        let confirmed = crate::hardware::ConfirmedHardwarePublicAccount::new_for_tests(
            native_descriptor,
            Address::repeat_byte(0x44),
        );
        let native = vault
            .add_hardware_public_account(&view, &confirmed, None)
            .unwrap();
        let native_auth = authorize(
            &owner,
            &view,
            &descriptor,
            HardwareExecutorAction::Execute(operation),
        );
        assert!(
            owner
                .prepare_operation(
                    operation,
                    ExecutorDelivery::SelfBroadcast {
                        sender: native.address,
                        sponsored: false
                    },
                    &native_auth,
                    &[],
                    None
                )
                .await
                .is_err()
        );
        let no_payer = authorize(
            &owner,
            &view,
            &descriptor,
            HardwareExecutorAction::Execute(operation),
        );
        assert!(
            owner
                .prepare_operation(operation, delivery(), &no_payer, &[], None)
                .await
                .is_err()
        );
        assert!(store.records().unwrap().is_empty());
        assert!(rpc.state.requests.lock().unwrap().is_empty());
        assert!(
            owner
                .hardware_authorization_request(
                    view.clone(),
                    HardwareExecutorAction::Execute(operation)
                )
                .unwrap()
                .with_gas_payer(
                    payer.public_account_uuid.clone(),
                    Zeroizing::new("wrong password".into()),
                    None
                )
                .is_err()
        );
        let request = owner
            .hardware_authorization_request(
                view.clone(),
                HardwareExecutorAction::Execute(operation),
            )
            .unwrap()
            .with_gas_payer(
                payer.public_account_uuid.clone(),
                Zeroizing::new(TEST_PASSWORD.into()),
                None,
            )
            .unwrap();
        let authorization = DesktopPrivateSpendAuthorization::HardwareExecutor(Box::new(
            request.complete(&descriptor, &[42; 32]).unwrap(),
        ));
        assert!(
            owner
                .prepare_operation(
                    operation,
                    ExecutorDelivery::SelfBroadcast {
                        sender: Address::repeat_byte(0x55),
                        sponsored: false
                    },
                    &authorization,
                    &[],
                    None,
                )
                .await
                .is_err()
        );
        assert!(store.records().unwrap().is_empty());
        let prepared = owner
            .prepare_operation(operation, delivery(), &authorization, &[], None)
            .await
            .unwrap();
        let spare = store.spare().unwrap().unwrap();
        assert_eq!(rpc.count_for(spare.address().unwrap()), 0);
        assert!(
            owner
                .prepare_operation(
                    ExecutorOperationId::random().unwrap(),
                    delivery(),
                    &authorization,
                    &[],
                    None
                )
                .await
                .is_err()
        );
        let input = Utxo::new(
            broadcaster_core::notes::Note::new_change(
                view.scan_keys().master_public_key,
                Address::repeat_byte(2),
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
            to: prepared.context().executor,
            data: RelayAdapt7702::executeCall {
                _transactions: vec![Transaction {
                    proof: SnarkProof::default(),
                    merkleRoot: B256::ZERO,
                    nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
                    commitments: Vec::new(),
                    boundParams: BoundParams::new_transact(
                        0,
                        0,
                        1,
                        Vec::new(),
                        prepared.context().executor,
                        B256::ZERO,
                    ),
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
        let issued = owner
            .issue_operation(
                &prepared,
                &call,
                std::slice::from_ref(&input),
                &authorization,
            )
            .await
            .unwrap();
        let signed =
            RelayAdapt7702::executeCall::abi_decode(issued.transaction().input.input().unwrap())
                .unwrap();
        let signature = alloy::primitives::Signature::try_from(signed._signature.as_ref()).unwrap();
        assert_eq!(
            signature
                .recover_address_from_prehash(&prepared.context().signing_hash(&call).unwrap())
                .unwrap(),
            prepared.context().executor
        );
        let delegation = &issued.transaction().authorization_list.as_ref().unwrap()[0];
        assert_eq!(
            delegation.recover_authority().unwrap(),
            prepared.context().executor
        );
        assert_eq!(delegation.inner().nonce, 0); // The separate payer consumes its own nonce.
        assert_eq!(delegation.inner().chain_id, U256::ONE);
        assert_eq!(
            store.records().unwrap()[0].issued()[0].hash(),
            issued.payload_hash()
        );
        // Fee convergence may rebuild a proof and issue another payload under this approval.
        let mut repriced = RelayAdapt7702::executeCall::abi_decode(&call.data).unwrap();
        repriced._transactions[0]
            .commitments
            .push(B256::repeat_byte(8));
        let repriced = railgun_wallet::TransactionCall {
            to: call.to,
            data: repriced.abi_encode().into(),
        };
        let revised = owner
            .issue_operation(&prepared, &repriced, &[input], &authorization)
            .await
            .unwrap();
        assert_ne!(revised.payload_hash(), issued.payload_hash());
        assert_eq!(store.records().unwrap()[0].issued().len(), 2);
        drop(authorization);
        owner.shutdown().await;
        let reads = rpc.state.requests.lock().unwrap().len();
        let owner = Arc::new(
            ExecutorOwner::new(
                1,
                db.clone(),
                view.clone(),
                chain(&rpc),
                HttpContext::direct_for_tests(),
            )
            .unwrap(),
        );
        assert_eq!(rpc.state.requests.lock().unwrap().len(), reads);
        assert_eq!(store.spare().unwrap().unwrap().address(), spare.address());
        let next_operation = ExecutorOperationId::random().unwrap();
        let next = DesktopPrivateSpendAuthorization::HardwareExecutor(Box::new(
            owner
                .hardware_authorization_request(
                    view.clone(),
                    HardwareExecutorAction::Execute(next_operation),
                )
                .unwrap()
                .with_gas_payer(
                    payer.public_account_uuid.clone(),
                    Zeroizing::new(TEST_PASSWORD.into()),
                    None,
                )
                .unwrap()
                .complete(&descriptor, &[42; 32])
                .unwrap(),
        ));
        let next = owner
            .prepare_operation(next_operation, delivery(), &next, &[], None)
            .await
            .unwrap();
        assert_eq!(next.context().executor, spare.address().unwrap());
        assert_eq!(
            rpc.count_for(store.spare().unwrap().unwrap().address().unwrap()),
            0
        );
        // Restore/registration exercise another historical identity without issued work.
        let restored = authorize(
            &owner,
            &view,
            &descriptor,
            HardwareExecutorAction::Restore(27..28),
        );
        owner.discover_range(&restored, 27..28).await.unwrap();
        assert!(owner.discover_range(&restored, 27..28).await.is_err());
        let record = store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.index() == 27)
            .unwrap();
        let registration = authorize(
            &owner,
            &view,
            &descriptor,
            HardwareExecutorAction::Register(record.operation()),
        );
        let reads = rpc.state.requests.lock().unwrap().len();
        let account = owner
            .register_public_account(record.operation(), &registration)
            .await
            .unwrap();
        assert_eq!(rpc.state.requests.lock().unwrap().len(), reads);
        assert!(account.hardware_descriptor.is_none());
        assert!(matches!(
            account.source,
            PublicAccountSource::ExecutorDerived(_)
        ));
        let repeat = authorize(
            &owner,
            &view,
            &descriptor,
            HardwareExecutorAction::Register(record.operation()),
        );
        assert_eq!(
            owner
                .register_public_account(record.operation(), &repeat)
                .await
                .unwrap()
                .public_account_uuid,
            account.public_account_uuid
        );
        let mut profile = view.hardware_profile_session().unwrap().clone();
        profile.trezor_session_id = Some(vec![1, 2, 3]);
        let refreshed = Arc::new(view.clone_with_hardware_profile_session(profile));
        let request = |authorization, view_session| WalletConnectPersonalSignRequest {
            chain_id: 1,
            executor_owner: Some(owner.clone()),
            request_control: None,
            view_session,
            vault_store: vault.clone(),
            authorization: Some(authorization),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: account.public_account_uuid.clone(),
            message: b"hardware-derived executor".to_vec(),
            event_tx: None,
        };
        assert!(
            walletconnect_sign_personal_message(request(password, refreshed.clone()))
                .await
                .is_err()
        );
        assert!(
            walletconnect_sign_personal_message(request(registration, refreshed.clone()))
                .await
                .is_err()
        );
        let public = || {
            authorize(
                &owner,
                &refreshed,
                &descriptor,
                HardwareExecutorAction::Public {
                    account: account.public_account_uuid.clone(),
                    operation: record.operation(),
                },
            )
        };
        walletconnect_sign_personal_message(request(public(), refreshed.clone()))
            .await
            .unwrap();
        let gas = authorize(
            &owner,
            &refreshed,
            &descriptor,
            HardwareExecutorAction::GasPayment {
                account: account.public_account_uuid.clone(),
                operation: record.operation(),
            },
        );
        // A private-only grant cannot serve as this account's payer, and admitting
        // its gas signer consumes the same action's executor authority.
        let (signer, guard) = owner
            .admit_authorized_gas_signer(&refreshed, &account, &gas)
            .await
            .unwrap();
        assert_eq!(signer.address(), account.address);
        drop(guard);
        assert!(
            owner
                .admit_authorized_gas_signer(&refreshed, &account, &gas)
                .await
                .is_err()
        );
        let reopened = Arc::new(load_test_hardware_view_session(
            &vault,
            TEST_WALLET_ID,
            &descriptor,
        ));
        assert!(
            walletconnect_sign_personal_message(request(public(), reopened.clone()))
                .await
                .is_err()
        );
        assert!(
            owner
                .hardware_authorization_request(reopened, HardwareExecutorAction::Restore(0..1))
                .is_err()
        );
        let delayed = owner
            .hardware_authorization_request(view.clone(), HardwareExecutorAction::Restore(0..1))
            .unwrap();
        owner.shutdown().await;
        assert!(delayed.ensure_active().is_err());
        assert!(delayed.complete(&descriptor, &[42; 32]).is_err());
        drop(owner);
        drop(store);
        drop(view);
        drop(refreshed);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn hardware_executor_recovery_consumes_approval_and_preserves_signed_history_on_failure() {
    let rpc = Rpc::start().await;
    rpc.state
        .native_balance
        .store(1_000_000_000, Ordering::Relaxed);
    let (root, db, vault) = desktop_store_with_vault();
    let descriptor = test_hardware_descriptor(0);
    let view = hardware_view(&vault, &descriptor);
    let owner = Arc::new(
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain(&rpc),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    let restore = authorize(
        &owner,
        &view,
        &descriptor,
        HardwareExecutorAction::Restore(42..43),
    );
    owner.discover_range(&restore, 42..43).await.unwrap();
    let record = owner.records().unwrap().remove(0);
    let authorization = authorize(
        &owner,
        &view,
        &descriptor,
        HardwareExecutorAction::Recover(record.operation()),
    );
    let funding = crate::ExecutorRecoveryFunding::ExecutorNative {
        gas_fee: crate::PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 0,
        },
    };
    let prepared = owner
        .prepare_recovery(
            record.operation(),
            ExecutorAsset::Native,
            U256::from(100),
            funding.clone(),
            &authorization,
        )
        .await
        .unwrap();
    assert_eq!(prepared.source(), record.address().unwrap());
    assert_eq!(prepared.recipient(), view.receive_address().unwrap());
    assert!(
        owner
            .prepare_recovery(
                record.operation(),
                ExecutorAsset::Native,
                U256::from(101),
                funding.clone(),
                &authorization
            )
            .await
            .is_err()
    );
    assert!(
        owner
            .submit_native_recovery_batch(&prepared, &authorization, |_| {})
            .await
            .is_err()
    );
    let saved = owner.records().unwrap().remove(0);
    assert_eq!(
        saved.issued().len(),
        1,
        "signed batch survives rejected outer submission"
    );
    let signed =
        RelayAdapt7702::multicallCall::abi_decode(saved.issued()[0].context().calldata()).unwrap();
    let signature = alloy::primitives::Signature::try_from(signed._signature.as_ref()).unwrap();
    let hash = broadcaster_core::contracts::executor::multicall_signing_hash(
        signed._requireSuccess,
        &signed._calls,
        signed._nonce,
        1,
        prepared.source(),
    );
    assert_eq!(
        signature.recover_address_from_prehash(&hash).unwrap(),
        prepared.source()
    );
    assert!(
        owner
            .submit_native_recovery_batch(&prepared, &authorization, |_| {})
            .await
            .is_err()
    );
    assert_eq!(owner.records().unwrap()[0].issued().len(), 1);
    let fresh = authorize(
        &owner,
        &view,
        &descriptor,
        HardwareExecutorAction::Recover(record.operation()),
    );
    let address_data = crate::AddressData::try_from(&crate::RailgunAddress::from(
        view.receive_address().unwrap().as_str(),
    ))
    .unwrap();
    let candidate = crate::PublicBroadcasterCandidate {
        chain_id: 1,
        railgun_address: view.receive_address().unwrap(),
        identifier: None,
        token: Address::repeat_byte(2),
        fee: U256::ONE,
        fees_id: "hardware-recovery-test".into(),
        fee_expiration: std::time::SystemTime::now() + Duration::from_mins(1),
        reliability: 1.0,
        available_wallets: 1,
        version: "test".into(),
        relay_adapt: record.delegate(),
        relay_adapt_7702: Some(record.delegate()),
        required_poi_list_keys: Vec::new(),
        viewing_public_key: address_data.viewing_public_key,
        address_data,
        fee_policy_status: crate::BroadcasterFeePolicyStatus::UnknownAnchor,
    };
    let paid = owner
        .prepare_recovery(
            record.operation(),
            ExecutorAsset::Native,
            U256::from(100),
            crate::ExecutorRecoveryFunding::PublicBroadcaster {
                candidate: Box::new(candidate),
                maximum_private_fee: U256::from(1000),
            },
            &fresh,
        )
        .await
        .unwrap();
    assert!(matches!(
        paid.execution(),
        crate::ExecutorRecoveryExecution::PaidExecute { .. }
    ));
    assert!(fresh.signer(&vault, &view, "recovery private fee").is_ok());
    owner.shutdown().await;
    drop(owner);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
