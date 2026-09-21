use super::*;
use crate::public_wallet::{
    WalletConnectPersonalSignRequest, admitted_public_signer, walletconnect_sign_personal_message,
};
use crate::{DesktopPrivateSpendAuthorization, ExecutorOwner, HttpContext};
use alloy::network::TransactionBuilder as _;

fn chain() -> crate::settings::EffectiveChainConfig {
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    chain.rpc_route = crate::RpcChainRoute::new(1, Vec::<url::Url>::new());
    chain
}

fn authorization() -> DesktopPrivateSpendAuthorization {
    DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(TEST_PASSWORD.to_owned()))
}

fn restore(
    vault: &DesktopVaultStore,
    view: &Arc<DesktopViewSession>,
    db: &Arc<DbStore>,
    index: u32,
) -> ExecutorRecord {
    let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
    let (_, signer) = vault
        .executor_spend_signers_for_session(&mut grant, view, None, 1, index)
        .unwrap();
    ExecutorStore::new(db.clone(), view.clone(), 1)
        .unwrap()
        .restore_index(
            index,
            signer.address(),
            chain().accepted_executor_profile().unwrap().delegate(),
            &[],
        )
        .unwrap()
}

#[tokio::test]
async fn executor_public_registration_is_atomic_idempotent_and_preserves_custody_after_restart() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let ordinary = vault.list_public_accounts_for_session(&view, true).unwrap();
    let ordinary_bytes = db
        .get_desktop_wallet_vault_record(&public_account_metadata_record_key(
            &ordinary[0].public_account_uuid,
        ))
        .unwrap();
    let record = restore(&vault, &view, &db, 42);
    assert!(
        vault
            .list_active_public_accounts_for_session(&view)
            .unwrap()
            .iter()
            .all(|account| Some(account.address) != record.address()),
        "unregistered records stay out of normal Public lists"
    );
    let operation = record.operation();
    let owner = Arc::new(
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain(),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    let auth = authorization();
    let (first, second) = tokio::join!(
        owner.register_public_account(operation, &auth),
        owner.register_public_account(operation, &auth)
    );
    let first = first.unwrap();
    assert_eq!(first, second.unwrap());
    assert_eq!(first.address, record.address().unwrap());
    let mut other_chain = chain();
    other_chain.chain_id = 137;
    other_chain.rpc_route =
        crate::RpcChainRoute::new(137, vec!["http://127.0.0.1:9".parse::<url::Url>().unwrap()]);
    let snapshot = crate::refresh_public_balances(
        137,
        std::slice::from_ref(&first),
        &other_chain,
        None,
        &HttpContext::direct_for_tests(),
    )
    .await
    .unwrap();
    assert!(
        snapshot.accounts.is_empty(),
        "off-chain refresh must not query the registered address"
    );
    assert!(
        vault.list_gateway_permissions(&view).unwrap().is_empty(),
        "registration never creates a site grant"
    );
    let origin = crate::RpcOrigin::dapp("registered-test-peer", "https://example.test").unwrap();
    assert!(
        vault
            .grant_gateway_permission(&view, &origin, &first.public_account_uuid, 137)
            .is_err()
    );
    let permission = vault
        .grant_gateway_permission(&view, &origin, &first.public_account_uuid, 1)
        .unwrap();
    assert_eq!(permission.public_account_uuid, first.public_account_uuid);
    let namespaces = crate::walletconnect::negotiate_walletconnect_namespaces(
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::from([1, 137]),
        first.address,
        first.source,
    )
    .unwrap();
    assert_eq!(
        namespaces.approved_namespaces["eip155"].chains,
        vec!["eip155:1"]
    );

    assert_eq!(
        db.get_desktop_wallet_vault_record(&public_account_metadata_record_key(
            &ordinary[0].public_account_uuid
        ))
        .unwrap(),
        ordinary_bytes
    );
    assert!(
        db.get_desktop_wallet_vault_record(&public_account_secret_record_key(
            &first.public_account_uuid
        ))
        .unwrap()
        .is_none()
    );
    assert!(
        vault
            .public_account_signing_key(
                &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
                &view,
                &first.public_account_uuid
            )
            .is_err(),
        "unguarded key access must remain unavailable"
    );
    assert!(
        admitted_public_signer(
            &vault,
            &view,
            Some(TEST_PASSWORD),
            &first.public_account_uuid,
            None,
            None,
            None,
            None,
            1
        )
        .await
        .is_err()
    );
    assert!(
        admitted_public_signer(
            &vault,
            &view,
            Some(TEST_PASSWORD),
            &first.public_account_uuid,
            None,
            None,
            None,
            Some(&owner),
            137
        )
        .await
        .is_err()
    );
    // An imported account with no issued payloads needs no history RPC, even on restart.
    owner.shutdown().await;
    let view2 = Arc::new(
        vault
            .load_view_session(TEST_PASSWORD, TEST_WALLET_ID)
            .unwrap(),
    );
    let replacement = Arc::new(
        ExecutorOwner::new(
            1,
            db.clone(),
            view2.clone(),
            chain(),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    assert!(
        admitted_public_signer(
            &vault,
            &view,
            Some(TEST_PASSWORD),
            &first.public_account_uuid,
            None,
            None,
            None,
            Some(&replacement),
            1
        )
        .await
        .is_err()
    );
    let signer = admitted_public_signer(
        &vault,
        &view2,
        Some(TEST_PASSWORD),
        &first.public_account_uuid,
        None,
        None,
        None,
        Some(&replacement),
        1,
    )
    .await
    .unwrap();
    assert_eq!(signer.address(), first.address);
    let mut registration = Box::pin(replacement.register_public_account(operation, &auth));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut registration)
            .await
            .is_err(),
        "registration cannot race an admitted Public signer"
    );
    replacement.close();
    assert!(
        signer
            .sign_transaction_request(
                alloy::rpc::types::TransactionRequest::default()
                    .from(first.address)
                    .with_chain_id(1),
                "test"
            )
            .await
            .is_err()
    );
    drop(signer);
    assert!(registration.await.is_err());
    let current = Arc::new(
        ExecutorOwner::new(
            2,
            db.clone(),
            view2.clone(),
            chain(),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    vault
        .deactivate_derived_public_account(&view2, &first.public_account_uuid)
        .unwrap();
    assert!(
        admitted_public_signer(
            &vault,
            &view2,
            Some(TEST_PASSWORD),
            &first.public_account_uuid,
            None,
            None,
            None,
            Some(&current),
            1
        )
        .await
        .is_err()
    );
    assert_eq!(
        current
            .register_public_account(operation, &auth)
            .await
            .unwrap()
            .public_account_uuid,
        first.public_account_uuid
    );
    let store = ExecutorStore::new(db.clone(), view2.clone(), 1).unwrap();
    let restored = store.records().unwrap().remove(0);
    assert_eq!(restored.operation(), operation);
    assert_eq!(restored.restored_at(), record.restored_at());
    assert!(restored.is_retired());
    assert_eq!(
        restored.public_account_uuid(),
        Some(first.public_account_uuid.as_str())
    );
    assert!(
        store
            .reserve(
                ExecutorOperationId::random().unwrap(),
                record.delegate(),
                None,
                &[]
            )
            .unwrap()
            .index()
            > 42
    );
    assert_eq!(
        vault
            .list_public_accounts_for_session(&view2, true)
            .unwrap()
            .len(),
        ordinary.len() + 1
    );
    current.shutdown().await;
    drop((current, replacement, owner, store, view2, view, vault, db));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn executor_public_message_signing_requires_the_original_passphrase_session() {
    let (root, db, vault) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&vault, "base", "Base");
    let unlocked = vault.unlock_view(TEST_PASSWORD).unwrap();
    let CreateSoftwareContextResult::Created {
        protected_seed_session,
        ..
    } = vault
        .create_software_context(
            &unlocked,
            &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
            base.wallet_id(),
            "child",
            4,
            "Child",
            Zeroizing::new("test passphrase".into()),
            Zeroizing::new("test passphrase".into()),
            SoftwareContextSyncIntent::RecoverExisting,
            &[],
            VaultSessionId::from_bytes([31; 16]),
        )
        .unwrap()
    else {
        panic!("new context");
    };
    let protected = Arc::new(protected_seed_session);
    let view = Arc::new(vault.load_view_session(TEST_PASSWORD, "child").unwrap());
    let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
    let (_, expected) = vault
        .executor_spend_signers_for_session(&mut grant, &view, Some(&protected), 1, 7)
        .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let record = store
        .restore_index(
            7,
            expected.address(),
            chain().accepted_executor_profile().unwrap().delegate(),
            &[],
        )
        .unwrap();
    let owner = Arc::new(
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain(),
            HttpContext::direct_for_tests(),
        )
        .unwrap(),
    );
    assert!(
        owner
            .register_public_account(record.operation(), &authorization())
            .await
            .is_err()
    );
    let auth = DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed {
        password: Zeroizing::new(TEST_PASSWORD.into()),
        session: protected.clone(),
    };
    let account = owner
        .register_public_account(record.operation(), &auth)
        .await
        .unwrap();
    let message = b"Public recovery custody regression";
    let vault = Arc::new(vault);
    for with_seed in [false, true] {
        let result = walletconnect_sign_personal_message(WalletConnectPersonalSignRequest {
            chain_id: 1,
            executor_owner: Some(owner.clone()),
            request_control: None,
            view_session: view.clone(),
            vault_store: vault.clone(),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.into()),
                    with_seed.then(|| protected.clone()),
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: account.public_account_uuid.clone(),
            message: message.to_vec(),
            event_tx: None,
        })
        .await;
        if with_seed {
            let signature: alloy::primitives::Signature = result.unwrap().parse().unwrap();
            assert_eq!(
                signature.recover_address_from_msg(message).unwrap(),
                expected.address()
            );
        } else {
            assert!(result.is_err());
        }
    }
    owner.shutdown().await;
    drop((owner, store, view, vault, db));
    std::fs::remove_dir_all(root).unwrap();
}
