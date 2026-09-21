use super::*;
use crate::DesktopPrivateSpendAuthorization;
use crate::public_wallet::{
    WalletConnectPersonalSignRequest, admitted_public_signer, walletconnect_sign_personal_message,
};
use alloy::network::TransactionBuilder as _;

#[tokio::test]
async fn registration_rejects_prepared_executor_retry_and_shutdown_cancels_public_work() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
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
    let operation = ExecutorOperationId::random().unwrap();
    let prepared = owner
        .prepare_operation(
            operation,
            delivery(),
            &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                TEST_PASSWORD.into(),
            )),
            &[],
            None,
        )
        .await
        .unwrap();
    let before = rpc.state.requests.lock().unwrap().len();
    let auth =
        DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(TEST_PASSWORD.into()));
    let account = owner
        .register_public_account(operation, &auth)
        .await
        .unwrap();
    assert_eq!(
        rpc.state.requests.lock().unwrap().len(),
        before,
        "registration performs no RPC"
    );
    let call = railgun_wallet::TransactionCall {
        to: account.address,
        data: Bytes::new(),
    };
    assert!(
        owner
            .issue_operation(
                &prepared,
                &call,
                &[],
                &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                    TEST_PASSWORD.into()
                ))
            )
            .await
            .is_err()
    );
    assert!(
        owner
            .prepare_operation(
                operation,
                delivery(),
                &crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
                    TEST_PASSWORD.into()
                )),
                &[],
                None
            )
            .await
            .is_err()
    );
    assert!(owner.records().unwrap()[0].issued().is_empty());
    let signer = admitted_public_signer(
        &vault,
        &view,
        Some(TEST_PASSWORD),
        &account.public_account_uuid,
        None,
        None,
        None,
        Some(&owner),
        1,
    )
    .await
    .unwrap();
    let work = async {
        let result: eyre::Result<()> = signer.while_active(std::future::pending()).await;
        assert!(result.is_err());
        drop(signer);
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(work, owner.shutdown());
    })
    .await
    .expect("shutdown cancels blocked Public work and releases admission");
    drop((owner, view, vault, db));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn public_signing_reconciles_issued_payloads_and_reorged_ordinary_recovery() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let vault = Arc::new(vault);
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let record = store
        .reserve(
            operation,
            chain(&rpc).accepted_executor_profile().unwrap().delegate(),
            None,
            &[],
        )
        .unwrap();
    let (_, derived) = vault
        .executor_spend_signers_for_session(
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            &view,
            None,
            1,
            record.index(),
        )
        .unwrap();
    let address = derived.address();
    store.bind_address(operation, address).unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(9, B256::repeat_byte(9)), U256::ZERO);
    store.reconcile(operation, observed, &[]).unwrap();
    let payload = IssuedExecutorPayload::new(
        U256::ZERO,
        record.delegate(),
        B256::repeat_byte(4),
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadContext::new(Bytes::from_static(b"issued"), observed, Vec::new()),
    );
    store.record_issued(operation, payload.clone()).unwrap();
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
    let account = owner
        .register_public_account(
            operation,
            &DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(TEST_PASSWORD.into())),
        )
        .await
        .unwrap();
    let request = |account: &PublicAccountMetadata| WalletConnectPersonalSignRequest {
        chain_id: 1,
        executor_owner: Some(owner.clone()),
        request_control: None,
        view_session: view.clone(),
        vault_store: vault.clone(),
        authorization: Some(
            crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                Zeroizing::new(TEST_PASSWORD.into()),
                None,
            ),
        ),
        trezor_app_passphrase: None,
        trezor_pin_matrix_provider: None,
        public_account_uuid: account.public_account_uuid.clone(),
        message: b"guarded dapp request".to_vec(),
        event_tx: None,
    };
    let error = walletconnect_sign_personal_message(request(&account))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("earlier signed operation"));
    // Advancing the account/execution nonce without a canonical winner is not resolution.
    rpc.state.used.lock().unwrap().insert(address);
    assert!(
        walletconnect_sign_personal_message(request(&account))
            .await
            .is_err()
    );
    // A saved successful inclusion must be revalidated, including in the same owner.
    store
        .reconcile(
            operation,
            ExecutorNonceObservation::new(observed.block(), U256::ONE),
            &[(
                payload.hash(),
                ExecutorPayloadInclusion::new(
                    observed.block(),
                    B256::repeat_byte(5),
                    ExecutorExecutionResult::Executed,
                ),
            )],
        )
        .unwrap();
    assert!(!store.records().unwrap()[0].has_unresolved_issued_work());
    rpc.state.used.lock().unwrap().remove(&address);
    assert!(
        walletconnect_sign_personal_message(request(&account))
            .await
            .is_err()
    );
    let (_, recovered) = vault
        .executor_spend_signers_for_session(
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            &view,
            None,
            1,
            27,
        )
        .unwrap();
    let address = recovered.address();
    let ordinary = store
        .restore_index(27, address, record.delegate(), &[])
        .unwrap();
    let operation = ordinary.operation();
    let account = owner
        .register_public_account(
            operation,
            &DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(TEST_PASSWORD.into())),
        )
        .await
        .unwrap();
    assert!(
        walletconnect_sign_personal_message(request(&account))
            .await
            .is_ok()
    );
    let tx_hash = B256::repeat_byte(6);
    let tx = alloy::rpc::types::TransactionRequest::default()
        .from(address)
        .to(Address::repeat_byte(7))
        .with_chain_id(1)
        .nonce(1)
        .with_gas_limit(65_000)
        .max_fee_per_gas(2)
        .max_priority_fee_per_gas(1);
    store
        .record_recovery_transaction(
            operation,
            IssuedExecutorRecoveryTransaction::new(
                ExecutorOperationId::random().unwrap(),
                0,
                ExecutorRecoveryStepKind::Shield,
                tx,
                tx_hash,
                observed.block(),
            ),
        )
        .unwrap();
    store
        .reconcile_recovery(
            operation,
            observed.block(),
            &[(
                tx_hash,
                ExecutorPayloadInclusion::new(
                    observed.block(),
                    tx_hash,
                    ExecutorExecutionResult::Executed,
                ),
            )],
        )
        .unwrap();
    // The local success was reorganized away: empty canonical blocks cannot authorize signing.
    assert!(
        walletconnect_sign_personal_message(request(&account))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .records()
            .unwrap()
            .iter()
            .find(|item| item.operation() == operation)
            .unwrap()
            .recovery_transaction_status(tx_hash),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    owner.shutdown().await;
    drop((owner, store, view, vault, db));
    std::fs::remove_dir_all(root).unwrap();
}
