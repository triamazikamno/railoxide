use super::super::*;
use super::helpers::*;
use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, U256};
use railgun_wallet::{Utxo, UtxoCommitmentKind, UtxoSource};

mod public_account;
mod spare;

fn namespace(
    store: &DesktopVaultStore,
    view: &DesktopViewSession,
    chain: u64,
) -> WalletChainMetadataBundle {
    store
        .wallet_chain_metadata_for_session(view, 0, chain, &Address::ZERO.to_string(), 0)
        .unwrap()
}

#[test]
fn executor_records_saved_before_display_metadata_remain_readable() {
    // Match the original named MessagePack record, including Alloy's binary
    // address encoding. Display metadata must not make retained accounts unreadable.
    #[derive(serde::Serialize)]
    struct EarlierRecord {
        version: u32,
        derivation: ExecutorDerivationScheme,
        origin: ExecutorRecordOrigin,
        operation: ExecutorOperationId,
        index: u32,
        address: Address,
        delegate: Address,
        retired: bool,
        issued: Vec<IssuedExecutorPayload>,
    }
    let earlier = EarlierRecord {
        version: 1,
        derivation: ExecutorDerivationScheme::Railgun7702V1,
        origin: ExecutorRecordOrigin::Discovered,
        operation: ExecutorOperationId::random().unwrap(),
        index: 42,
        address: Address::repeat_byte(3),
        delegate: Address::repeat_byte(4),
        retired: true,
        issued: Vec::new(),
    };
    let record: ExecutorRecord = rmp_serde::from_slice(&rmp_serde::to_vec_named(&earlier).unwrap())
        .expect("earlier records remain readable without rewriting the database");
    assert_eq!(record.operation(), earlier.operation);
    assert_eq!(record.index(), earlier.index);
    assert_eq!(record.address(), Some(earlier.address));
    assert!(record.is_retired());
    assert!(!record.is_hidden());
    assert!(record.created_at().is_none() && record.restored_at().is_none());
    assert!(record.purpose_summary().is_none() && record.assets().is_empty());
    assert_eq!(
        rmp_serde::from_slice::<ExecutorRecord>(&rmp_serde::to_vec_named(&record).unwrap(),)
            .unwrap(),
        record
    );
}

#[tokio::test]
async fn executor_discovery_restores_unknown_high_indices_without_lowering_the_allocation_floor() {
    use crate::{ExecutorOwner, HttpContext};

    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    // Unavailable RPC cannot erase restored accounts or report an empty address family.
    chain.rpc_route =
        crate::RpcChainRoute::new(1, vec![url::Url::parse("http://127.0.0.1:1").unwrap()]);
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let records = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
    let report = owner
        .discover_range(&mut grant, None, 5_000_000..5_000_002)
        .await
        .unwrap();
    assert_eq!(report.unavailable(), 2);
    let accounts = records.records().unwrap();
    assert_eq!(report.range(), 5_000_000..5_000_002);
    assert_eq!(accounts.len(), 2);
    assert!(
        accounts
            .iter()
            .all(|account| account.use_check().is_unavailable() && account.is_retired())
    );
    let restored_at = accounts[0].restored_at();
    assert!(restored_at.is_some());
    assert!(accounts[0].created_at().is_none());
    assert!(accounts[0].purpose_summary().is_none());
    let identities = accounts
        .iter()
        .map(|account| (account.operation(), account.address().unwrap()))
        .collect::<Vec<_>>();
    assert_ne!(identities[0].1, identities[1].1);
    assert_eq!(records.next_index().unwrap(), 5_000_002);
    owner.shutdown().await;
    drop(owner);

    let owner = ExecutorOwner::new(
        1,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
    let restored = owner
        .discover_range(&mut grant, None, 5_000_000..5_000_001)
        .await
        .unwrap();
    assert_eq!(restored.unavailable(), 1);
    let restored = records.records().unwrap();
    assert_eq!(restored[0].operation(), identities[0].0);
    assert_eq!(restored[0].restored_at(), restored_at);
    assert!(restored[0].assets().is_empty());
    let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
    owner.discover_range(&mut grant, None, 2..3).await.unwrap();
    assert_eq!(records.next_index().unwrap(), 5_000_002);
    let count = records.records().unwrap().len();
    let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
    assert!(
        owner
            .discover_range(&mut grant, None, 10..75)
            .await
            .is_err()
    );
    assert_eq!(records.records().unwrap().len(), count);
    owner.shutdown().await;
    drop(owner);
    drop(records);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn executor_ordinary_recovery_handoff_survives_restart_and_reorg_without_recycling() {
    #[derive(serde::Serialize)]
    struct LegacyInclusion {
        block: BlockNumHash,
        transaction_hash: B256,
        result: ExecutorExecutionResult,
    }
    use alloy::rpc::types::TransactionRequest;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let address = Address::repeat_byte(1);
    let record = store
        .restore_index(5_000_000, address, Address::repeat_byte(2), &[])
        .unwrap();
    let floor = store.next_index().unwrap();
    let mut request = TransactionRequest::default()
        .to(Address::repeat_byte(3))
        .input(Bytes::from_static(b"approved recovery fixture").into());
    request.from = Some(address);
    request.chain_id = Some(1);
    request.nonce = Some(7);
    request.gas = Some(100_000);
    request.max_fee_per_gas = Some(2);
    request.max_priority_fee_per_gas = Some(1);
    let hash = B256::repeat_byte(4);
    let observed = BlockNumHash::new(10, B256::repeat_byte(10));
    let recovery = ExecutorOperationId::random().unwrap();
    let transaction = IssuedExecutorRecoveryTransaction::new(
        recovery,
        0,
        ExecutorRecoveryStepKind::ApproveErc20,
        request.clone(),
        hash,
        observed,
    );
    store
        .record_recovery_transaction(record.operation(), transaction.clone())
        .unwrap();
    store
        .record_recovery_transaction(record.operation(), transaction)
        .unwrap();
    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let restored = store.records().unwrap().remove(0);
    assert_eq!(restored.recovery_transactions().len(), 1);
    assert_eq!(restored.recovery_transactions()[0].transaction(), &request);
    assert_eq!(
        restored.recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    let included = BlockNumHash::new(11, B256::repeat_byte(11));
    store
        .reconcile_recovery(
            record.operation(),
            included,
            &[(
                hash,
                ExecutorPayloadInclusion::new(included, hash, ExecutorExecutionResult::Executed),
            )],
        )
        .unwrap();
    let cold = store.invalidate_observation(record.operation()).unwrap();
    assert_eq!(
        cold.recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    assert_eq!(
        cold.recorded_recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Executed)
    );
    assert!(cold.recovery_transactions()[0].inclusion().is_some());
    let reorged = store
        .reconcile_recovery(record.operation(), observed, &[])
        .unwrap();
    assert_eq!(
        reorged.recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    assert!(reorged.recovery_transactions()[0].inclusion().is_none());
    assert_eq!(store.next_index().unwrap(), floor);
    let mut replacement_request = request.clone();
    replacement_request.max_fee_per_gas = Some(3);
    let replacement_hash = B256::repeat_byte(5);
    store
        .record_recovery_transaction(
            record.operation(),
            IssuedExecutorRecoveryTransaction::new(
                recovery,
                0,
                ExecutorRecoveryStepKind::ApproveErc20,
                replacement_request,
                replacement_hash,
                observed,
            ),
        )
        .unwrap();
    let replacement_inclusion = ExecutorPayloadInclusion::new(
        included,
        replacement_hash,
        ExecutorExecutionResult::Reverted,
    );
    let replaced = store
        .reconcile_recovery(
            record.operation(),
            included,
            &[(replacement_hash, replacement_inclusion)],
        )
        .unwrap();
    assert_eq!(
        replaced.recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Invalidated {
            winner: replacement_hash
        })
    );
    assert_eq!(
        replaced.recovery_transaction_status(replacement_hash),
        Some(ExecutorPayloadStatus::Reverted)
    );
    let stale = store.invalidate_observation(record.operation()).unwrap();
    for (transaction, outcome) in [
        (
            hash,
            ExecutorPayloadStatus::Invalidated {
                winner: replacement_hash,
            },
        ),
        (replacement_hash, ExecutorPayloadStatus::Reverted),
    ] {
        assert_eq!(
            stale.recorded_recovery_transaction_status(transaction),
            Some(outcome)
        );
        assert_eq!(
            stale.recovery_transaction_status(transaction),
            Some(ExecutorPayloadStatus::Uncertain)
        );
    }
    assert!(matches!(
        store.reconcile_recovery(
            record.operation(),
            included,
            &[
                (
                    hash,
                    ExecutorPayloadInclusion::new(
                        included,
                        hash,
                        ExecutorExecutionResult::Executed
                    )
                ),
                (replacement_hash, replacement_inclusion),
            ]
        ),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    let reopened = store
        .reconcile_recovery(record.operation(), observed, &[])
        .unwrap();
    assert_eq!(
        reopened.recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    request.value = Some(U256::ONE);
    let changed = IssuedExecutorRecoveryTransaction::new(
        recovery,
        0,
        ExecutorRecoveryStepKind::ApproveErc20,
        request,
        B256::repeat_byte(6),
        observed,
    );
    assert!(matches!(
        store.record_recovery_transaction(record.operation(), changed),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    // An atomic replacement consumes the same sender nonce even if its asset
    // execution reverts. Broadcaster or different-nonce receipts are not evidence.
    let nonce = ExecutorNonceObservation::new(observed, U256::ZERO);
    store.reconcile(record.operation(), nonce, &[]).unwrap();
    let batch = B256::repeat_byte(8);
    store
        .record_issued(
            record.operation(),
            IssuedExecutorPayload::new(
                U256::ZERO,
                record.delegate(),
                batch,
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"atomic recovery"),
                    nonce,
                    Vec::new(),
                ),
            ),
        )
        .unwrap();
    let batch_inclusion =
        ExecutorPayloadInclusion::new(included, batch, ExecutorExecutionResult::Reverted);
    for sender_nonce in [None, Some(8), Some(7)] {
        store
            .reconcile(
                record.operation(),
                ExecutorNonceObservation::new(included, U256::ZERO),
                &[(
                    batch,
                    batch_inclusion.with_executor_account_nonce(sender_nonce),
                )],
            )
            .unwrap();
        let current = store
            .reconcile_recovery(record.operation(), included, &[])
            .unwrap();
        assert_eq!(
            current.recovery_transaction_status(hash),
            Some(if sender_nonce == Some(7) {
                ExecutorPayloadStatus::Invalidated { winner: batch }
            } else {
                ExecutorPayloadStatus::Uncertain
            })
        );
    }
    let cold = store.invalidate_observation(record.operation()).unwrap();
    assert_eq!(
        cold.recorded_recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Invalidated { winner: batch })
    );
    assert_eq!(
        cold.recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    let partial = store
        .reconcile_recovery(record.operation(), included, &[])
        .unwrap();
    assert_eq!(
        partial.recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    let reorged = store.reconcile(record.operation(), nonce, &[]).unwrap();
    assert_eq!(
        reorged.recorded_recovery_transaction_status(hash),
        Some(ExecutorPayloadStatus::Uncertain)
    );

    // Stored inclusions written before sender-nonce evidence remain readable.
    let legacy = rmp_serde::to_vec_named(&LegacyInclusion {
        block: included,
        transaction_hash: batch,
        result: ExecutorExecutionResult::Reverted,
    })
    .unwrap();
    let decoded: ExecutorPayloadInclusion = rmp_serde::from_slice(&legacy).unwrap();
    assert_eq!(decoded, batch_inclusion);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn executor_reservations_survive_restart_and_never_recycle_or_enter_position_range() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let namespace = namespace(&vault, &view, 1);
    let store = ExecutorStore::new(db.clone(), view.clone(), namespace.chain_id).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let assets = [
        ExecutorAsset::Native,
        ExecutorAsset::Erc20(Address::repeat_byte(7)),
    ];
    let first = store
        .reserve(operation, Address::ZERO, Some("Unshield 0.5 WETH"), &assets)
        .unwrap();
    assert!(first.created_at().is_some());
    assert!(first.restored_at().is_none());
    let barrier = std::sync::Barrier::new(2);
    let highest_index = std::thread::scope(|scope| {
        let next = scope.spawn(|| {
            barrier.wait();
            store
                .reserve(
                    ExecutorOperationId::random().unwrap(),
                    Address::ZERO,
                    None,
                    &[],
                )
                .unwrap()
        });
        let concurrent = scope.spawn(|| {
            barrier.wait();
            store
                .reserve(
                    ExecutorOperationId::random().unwrap(),
                    Address::ZERO,
                    None,
                    &[],
                )
                .unwrap()
        });
        assert_eq!(
            store.reserve(operation, Address::ZERO, None, &[]).unwrap(),
            first
        );
        let second = next.join().unwrap();
        let third = concurrent.join().unwrap();
        assert_ne!(second.index(), third.index());
        assert_ne!(first.index(), second.index());
        assert_ne!(first.index(), third.index());
        second.index().max(third.index())
    });
    store.set_hidden(operation, true).unwrap();
    store.retire(operation).unwrap();
    drop(store);
    drop(view);
    drop(vault);
    drop(db);

    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let vault = DesktopVaultStore::from_db(db.clone());
    let view = Arc::new(
        vault
            .load_view_session(TEST_PASSWORD, TEST_WALLET_ID)
            .unwrap(),
    );
    let store = ExecutorStore::new(db.clone(), view.clone(), namespace.chain_id).unwrap();
    let retained = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert!(retained.is_retired() && retained.is_hidden());
    assert_eq!(retained.created_at(), first.created_at());
    assert_eq!(retained.purpose_summary(), first.purpose_summary());
    assert_eq!(retained.assets(), assets);
    let restored = store
        .restore_index(first.index(), Address::repeat_byte(4), Address::ZERO, &[])
        .unwrap();
    assert_eq!(restored.created_at(), first.created_at());
    assert_eq!(restored.purpose_summary(), first.purpose_summary());
    assert!(restored.restored_at().is_some());
    assert_eq!(
        store
            .restore_index(first.index(), Address::repeat_byte(4), Address::ZERO, &[])
            .unwrap(),
        restored
    );
    assert!(
        store
            .reserve(
                ExecutorOperationId::random().unwrap(),
                Address::ZERO,
                None,
                &[]
            )
            .unwrap()
            .index()
            > highest_index
    );
    store.raise_floor(1_000_000).unwrap();
    assert_eq!(
        store
            .reserve(
                ExecutorOperationId::random().unwrap(),
                Address::ZERO,
                None,
                &[]
            )
            .unwrap()
            .index(),
        1_000_064
    );
    store.raise_floor(4).unwrap();
    assert_eq!(store.next_index().unwrap(), 1_000_065);
    let recovered = store
        .restore_index(5_000_000, Address::repeat_byte(9), Address::ZERO, &[])
        .unwrap();
    assert!(recovered.is_retired());
    assert!(matches!(
        store.reserve(recovered.operation(), Address::ZERO, None, &[]),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    store
        .restore_index(4, Address::repeat_byte(8), Address::ZERO, &[])
        .unwrap();
    db.update_desktop_wallet_vault_records(
        &[super::super::executors::executor_allocation_key(
            view.wallet_id(),
            1,
        )],
        &[],
    )
    .unwrap();
    // Retained history establishes a floor even when the allocation record was lost.
    assert_eq!(store.next_index().unwrap(), 5_000_001);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn executor_payload_handoff_is_durable_and_does_not_duplicate_on_retry() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let namespace = namespace(&vault, &view, 1);
    let store = ExecutorStore::new(db.clone(), view.clone(), namespace.chain_id).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let delegate = Address::repeat_byte(1);
    store.reserve(operation, delegate, None, &[]).unwrap();
    let mut grant = vault.create_spend_grant(TEST_PASSWORD).unwrap();
    let (_, signer) = vault
        .executor_spend_signers_for_session(&mut grant, &view, None, 1, 0)
        .unwrap();
    let seed = bip39_seed_from_mnemonic(TEST_MNEMONIC, "").unwrap();
    assert_eq!(
        signer.address(),
        railgun_wallet::keys::derive_executor_signer(&seed, 0, 1, 0)
            .unwrap()
            .address()
    );
    store.bind_address(operation, signer.address()).unwrap();
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, Address::ZERO, U256::from(9), [7; 16]),
        2,
        3,
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let inputs = vec![ExecutorInputIdentity::from_utxo(&input)];
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::from(3));
    store.reconcile(operation, observed, &[]).unwrap();
    let original = IssuedExecutorPayload::new(
        U256::from(3),
        delegate,
        B256::repeat_byte(4),
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadContext::new(Bytes::from_static(b"original"), observed, inputs.clone()),
    );
    store.record_issued(operation, original.clone()).unwrap();
    let conflicting = ExecutorOperationId::random().unwrap();
    store.reserve(conflicting, delegate, None, &[]).unwrap();
    store
        .bind_address(conflicting, Address::repeat_byte(8))
        .unwrap();
    store.reconcile(conflicting, observed, &[]).unwrap();
    assert!(matches!(
        store.record_issued(conflicting, original.clone()),
        Err(ExecutorStoreError::InputReserved)
    ));
    store
        .record_submission(operation, original.hash(), B256::repeat_byte(5))
        .unwrap();
    store.record_issued(operation, original).unwrap();
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::from(3),
                delegate,
                B256::repeat_byte(6),
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(Bytes::from_static(b"recovery"), observed, Vec::new()),
            ),
        )
        .unwrap();
    assert!(matches!(
        store.record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::from(4),
                delegate,
                B256::repeat_byte(7),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"future"), observed, Vec::new()),
            )
        ),
        Err(ExecutorStoreError::OutstandingNonce)
    ));
    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), namespace.chain_id).unwrap();
    let records = store.records().unwrap();
    assert_eq!(records[0].issued().len(), 2);
    assert_eq!(
        records[0].issued()[0].transaction_hashes(),
        &[B256::repeat_byte(5)]
    );
    assert_eq!(
        records[0].issued()[1].purpose(),
        ExecutorPayloadPurpose::Recovery
    );
    assert_eq!(
        records[0].issued()[0].context().calldata().as_ref(),
        b"original"
    );
    assert!(records[0].reserved_inputs()[0].matches(&input));

    // Nonce advancement alone cannot establish a winner or release its inputs.
    let advanced =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::from(4));
    let future = IssuedExecutorPayload::new(
        U256::from(4),
        delegate,
        B256::repeat_byte(7),
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadContext::new(Bytes::from_static(b"future"), advanced, inputs.clone()),
    );
    let uncertain = store.reconcile(operation, advanced, &[]).unwrap();
    assert_eq!(
        uncertain.payload_status(B256::repeat_byte(4)),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    assert_eq!(uncertain.reserved_inputs(), inputs);
    assert!(matches!(
        store.record_issued(operation, future.clone()),
        Err(ExecutorStoreError::OutstandingNonce)
    ));

    let inclusion = |result| {
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(5),
            result,
        )
    };
    for result in [
        ExecutorExecutionResult::MissingEffects,
        ExecutorExecutionResult::Reverted,
    ] {
        let incomplete = store
            .reconcile(
                operation,
                advanced,
                &[(B256::repeat_byte(6), inclusion(result))],
            )
            .unwrap();
        assert_eq!(incomplete.reserved_inputs(), inputs);
        assert!(matches!(
            store.record_issued(operation, future.clone()),
            Err(ExecutorStoreError::OutstandingNonce)
        ));
    }
    let recovery = store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::from(4),
                delegate,
                B256::repeat_byte(8),
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"recover stranded assets"),
                    advanced,
                    Vec::new(),
                ),
            ),
        )
        .unwrap();
    assert_eq!(recovery.reserved_inputs(), inputs);
    assert_eq!(
        recovery.payload_status(B256::repeat_byte(4)),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    // A background block observation persists the winner for history without
    // freeing inputs or admitting another signature from a stale nonce snapshot.
    let history = store
        .record_history(
            operation,
            advanced.block(),
            &[(
                B256::repeat_byte(6),
                inclusion(ExecutorExecutionResult::Executed),
            )],
        )
        .unwrap();
    assert_eq!(
        history.recorded_payload_status(B256::repeat_byte(4)),
        Some(ExecutorPayloadStatus::Invalidated {
            winner: B256::repeat_byte(6)
        })
    );
    assert_eq!(
        history.payload_status(B256::repeat_byte(6)),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    assert_eq!(history.reserved_inputs(), inputs);
    let recovery_won = store
        .reconcile(
            operation,
            advanced,
            &[(
                B256::repeat_byte(6),
                inclusion(ExecutorExecutionResult::Executed),
            )],
        )
        .unwrap();
    assert_eq!(
        recovery_won.payload_status(B256::repeat_byte(4)),
        Some(ExecutorPayloadStatus::Invalidated {
            winner: B256::repeat_byte(6)
        })
    );
    assert!(recovery_won.reserved_inputs().is_empty());
    let cold_owner = crate::ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap(),
        crate::HttpContext::direct_for_tests(),
    )
    .unwrap();
    assert!(cold_owner.available_inputs(vec![input]).unwrap().is_empty());
    drop(cold_owner);

    // A reorg reopens pending state, and a subsequently observed original is
    // executed, never reported as cancelled because recovery was attempted.
    let reorganized = store.reconcile(operation, observed, &[]).unwrap();
    assert_eq!(reorganized.reserved_inputs(), inputs);
    assert_eq!(
        reorganized.payload_status(B256::repeat_byte(6)),
        Some(ExecutorPayloadStatus::Uncertain)
    );
    let original_won = store
        .reconcile(
            operation,
            advanced,
            &[(
                B256::repeat_byte(4),
                inclusion(ExecutorExecutionResult::Executed),
            )],
        )
        .unwrap();
    assert_eq!(
        original_won.payload_status(B256::repeat_byte(4)),
        Some(ExecutorPayloadStatus::Executed)
    );
    assert_eq!(
        original_won.payload_status(B256::repeat_byte(6)),
        Some(ExecutorPayloadStatus::Invalidated {
            winner: B256::repeat_byte(4)
        })
    );
    // Canonical execution can precede the actor's private-note projection.
    assert_eq!(original_won.reserved_inputs(), inputs);
    store.record_issued(operation, future).unwrap();
    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), namespace.chain_id).unwrap();
    let record = store.records().unwrap().remove(0);
    assert_eq!(record.issued().len(), 4);
    assert_eq!(
        record.payload_status(B256::repeat_byte(4)),
        Some(ExecutorPayloadStatus::Executed)
    );
    assert_eq!(record.reserved_inputs(), inputs);
    let stale = store.invalidate_observation(operation).unwrap();
    // Historical results survive loss of current authority. The winner only
    // supersedes competitors for its own nonce, not the next signed payload.
    for (hash, outcome) in [
        (B256::repeat_byte(4), ExecutorPayloadStatus::Executed),
        (
            B256::repeat_byte(6),
            ExecutorPayloadStatus::Invalidated {
                winner: B256::repeat_byte(4),
            },
        ),
        (B256::repeat_byte(7), ExecutorPayloadStatus::Uncertain),
    ] {
        assert_eq!(stale.recorded_payload_status(hash), Some(outcome));
        assert_eq!(
            stale.payload_status(hash),
            Some(ExecutorPayloadStatus::Uncertain)
        );
    }
    assert_eq!(
        stale.issued()[0].inclusion(),
        record.issued()[0].inclusion()
    );
    assert_eq!(stale.reserved_inputs(), inputs);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn executor_records_authenticate_identity_and_wallet_deletion_blocks_stale_writes() {
    let (root, db, vault) = desktop_store_with_vault();
    let wallet_id = generate_opaque_id().unwrap();
    let view = Arc::new(import_wallet_with_metadata(&vault, &wallet_id, "Wallet"));
    let other_view = Arc::new(import_wallet_with_metadata(&vault, "other-wallet", "Other"));
    let mut first_namespace = namespace(&vault, &view, 1);
    let other_namespace = namespace(&vault, &other_view, 1);
    let store = ExecutorStore::new(db.clone(), view.clone(), first_namespace.chain_id).unwrap();
    let other =
        ExecutorStore::new(db.clone(), other_view.clone(), other_namespace.chain_id).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    store.reserve(operation, Address::ZERO, None, &[]).unwrap();
    let prefix = super::super::executors::executor_operation_prefix(
        view.wallet_id(),
        first_namespace.chain_id,
    );
    let stored = db
        .list_desktop_wallet_vault_records(&prefix)
        .unwrap()
        .remove(0);
    let other_key = format!(
        "{}{}",
        super::super::executors::executor_operation_prefix(
            other_view.wallet_id(),
            other_namespace.chain_id
        ),
        operation.opaque_id()
    );
    db.put_desktop_wallet_vault_records(&[(other_key.clone(), stored.payload.clone())])
        .unwrap();
    assert!(matches!(
        other.records(),
        Err(ExecutorStoreError::Vault(VaultError::Decrypt))
    ));
    db.update_desktop_wallet_vault_records(&[other_key], &[])
        .unwrap();
    let chain_namespace = namespace(&vault, &view, 137);
    let chain_store =
        ExecutorStore::new(db.clone(), view.clone(), chain_namespace.chain_id).unwrap();
    let chain_key = format!(
        "{}{}",
        super::super::executors::executor_operation_prefix(
            view.wallet_id(),
            chain_namespace.chain_id
        ),
        operation.opaque_id()
    );
    db.put_desktop_wallet_vault_records(&[(chain_key.clone(), stored.payload.clone())])
        .unwrap();
    assert!(matches!(
        chain_store.records(),
        Err(ExecutorStoreError::Vault(VaultError::Decrypt))
    ));
    db.update_desktop_wallet_vault_records(&[chain_key], &[])
        .unwrap();
    drop(chain_store);
    let allocation_key = super::super::executors::executor_allocation_key(
        view.wallet_id(),
        first_namespace.chain_id,
    );
    let allocation = db
        .get_desktop_wallet_vault_record(&allocation_key)
        .unwrap()
        .unwrap();
    db.put_desktop_wallet_vault_records(&[(allocation_key.clone(), stored.payload.clone())])
        .unwrap();
    assert!(matches!(
        store.next_index(),
        Err(ExecutorStoreError::Vault(VaultError::Decrypt))
    ));
    db.put_desktop_wallet_vault_records(&[(allocation_key, allocation)])
        .unwrap();
    let substituted_key = format!(
        "{}{}",
        prefix,
        ExecutorOperationId::random().unwrap().opaque_id()
    );
    db.put_desktop_wallet_vault_records(&[(substituted_key.clone(), stored.payload)])
        .unwrap();
    assert!(matches!(
        store.records(),
        Err(ExecutorStoreError::Vault(VaultError::Decrypt))
    ));
    db.update_desktop_wallet_vault_records(&[substituted_key], &[])
        .unwrap();
    vault
        .reset_wallet_chain_cache_with_session(&view, &mut first_namespace, 0)
        .unwrap();
    assert_eq!(store.records().unwrap().len(), 1);
    let replacement_cache = vault
        .wallet_chain_metadata_for_session(&view, 0, 1, &Address::repeat_byte(8).to_string(), 0)
        .unwrap();
    assert_ne!(
        replacement_cache.wallet_chain_uuid,
        first_namespace.wallet_chain_uuid
    );
    let replacement_store =
        ExecutorStore::new(db.clone(), view.clone(), replacement_cache.chain_id).unwrap();
    assert_eq!(
        replacement_store.records().unwrap(),
        store.records().unwrap()
    );
    assert!(
        replacement_store
            .reserve(
                ExecutorOperationId::random().unwrap(),
                Address::ZERO,
                None,
                &[]
            )
            .unwrap()
            .index()
            > 0
    );
    drop(replacement_store);
    assert_eq!(
        vault
            .list_active_public_accounts_for_session(&view)
            .unwrap()
            .len(),
        1
    );
    vault.delete_wallet_for_session(&view, &wallet_id).unwrap();
    assert!(store.records().unwrap().is_empty());
    assert!(matches!(
        store.reserve(
            ExecutorOperationId::random().unwrap(),
            Address::ZERO,
            None,
            &[]
        ),
        Err(ExecutorStoreError::Unavailable)
    ));
    drop(other);
    drop(store);
    drop(other_view);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
