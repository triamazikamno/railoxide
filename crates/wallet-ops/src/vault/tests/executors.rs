use super::super::*;
use super::helpers::*;
use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, U256};
use railgun_wallet::{Utxo, UtxoCommitmentKind, UtxoSource};

mod public_account;
mod spare;
mod swap;

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
    #[derive(serde::Serialize)]
    enum EarlierPurpose {
        Operation,
        Recovery,
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
    assert!(record.swap().is_none() && record.swap_approval().is_none());
    assert!(!record.is_swap_setup_stopped());
    // Payload purposes persisted before swap hooks keep decoding unchanged.
    for (legacy, purpose) in [
        (EarlierPurpose::Operation, ExecutorPayloadPurpose::Operation),
        (EarlierPurpose::Recovery, ExecutorPayloadPurpose::Recovery),
    ] {
        assert_eq!(
            rmp_serde::from_slice::<ExecutorPayloadPurpose>(
                &rmp_serde::to_vec_named(&legacy).unwrap()
            )
            .unwrap(),
            purpose
        );
    }
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
    let authorization = crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
        TEST_PASSWORD.into(),
    ));
    let report = owner
        .discover_range(&authorization, 5_000_000..5_000_002)
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
    let authorization = crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
        TEST_PASSWORD.into(),
    ));
    let restored = owner
        .discover_range(&authorization, 5_000_000..5_000_001)
        .await
        .unwrap();
    assert_eq!(restored.unavailable(), 1);
    let restored = records.records().unwrap();
    assert_eq!(restored[0].operation(), identities[0].0);
    assert_eq!(restored[0].restored_at(), restored_at);
    assert!(restored[0].assets().is_empty());
    let authorization = crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
        TEST_PASSWORD.into(),
    ));
    owner.discover_range(&authorization, 2..3).await.unwrap();
    assert_eq!(records.next_index().unwrap(), 5_000_002);
    let count = records.records().unwrap().len();
    let authorization = crate::DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(
        TEST_PASSWORD.into(),
    ));
    assert!(owner.discover_range(&authorization, 10..75).await.is_err());
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
fn hardware_executor_reservations_survive_restart_and_never_recycle_or_enter_position_range() {
    for descriptor in [
        None,
        Some(test_hardware_descriptor(0)),
        Some(test_trezor_hardware_descriptor(0)),
    ] {
        let (root, db, vault) = desktop_store_with_vault();
        let view = match &descriptor {
            Some(descriptor) => spare::hardware::hardware_view(&vault, descriptor),
            None => Arc::new(import_wallet_with_metadata(
                &vault,
                TEST_WALLET_ID,
                "Wallet",
            )),
        };
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
        let view = Arc::new(match &descriptor {
            Some(descriptor) => load_test_hardware_view_session(&vault, TEST_WALLET_ID, descriptor),
            None => vault
                .load_view_session(TEST_PASSWORD, TEST_WALLET_ID)
                .unwrap(),
        });
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
    store.record_account_read(operation, observed).unwrap();
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
    store.record_account_read(conflicting, observed).unwrap();
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

    // Nonce advancement resolves every payload at the consumed nonce and names none of
    // them. It does not release their inputs.
    let advanced =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::from(4));
    let resolved = store.record_account_read(operation, advanced).unwrap();
    for hash in [4, 6] {
        assert_eq!(
            resolved.payload_state(B256::repeat_byte(hash)),
            Some(ExecutorPayloadState::Resolved)
        );
    }
    assert_eq!(resolved.reserved_inputs(), inputs);
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
    // Every payload at the consumed nonce keeps its notes until private sync has scanned
    // the block of that read.
    let settled = store
        .record_synced(operation, None, advanced.block().number)
        .unwrap();
    assert!(settled.reserved_inputs().is_empty());
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
    assert_eq!(
        cold_owner.available_inputs(vec![input]).unwrap().len(),
        1,
        "a settled nonce's payloads must not reserve their notes again after restart"
    );
    drop(cold_owner);

    // A later read that shows the nonce unconsumed again reopens pending state and lowers
    // the settled nonce.
    let reopened =
        ExecutorNonceObservation::new(BlockNumHash::new(13, B256::repeat_byte(13)), U256::from(3));
    let reorganized = store.record_account_read(operation, reopened).unwrap();
    assert_eq!(reorganized.reserved_inputs(), inputs);
    assert_eq!(
        reorganized.payload_state(B256::repeat_byte(6)),
        Some(ExecutorPayloadState::Pending)
    );
    // Consumed again, the nonce's payloads reserve their notes until private sync passes
    // the new block.
    let readvanced =
        ExecutorNonceObservation::new(BlockNumHash::new(14, B256::repeat_byte(14)), U256::from(4));
    let consumed = store.record_account_read(operation, readvanced).unwrap();
    assert_eq!(consumed.reserved_inputs(), inputs);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::from(4),
                delegate,
                B256::repeat_byte(7),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"future"),
                    readvanced,
                    inputs.clone(),
                ),
            ),
        )
        .unwrap();
    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), namespace.chain_id).unwrap();
    let record = store.records().unwrap().remove(0);
    assert_eq!(record.issued().len(), 4);
    assert_eq!(record.reserved_inputs(), inputs);
    let stale = store.invalidate_observation(operation).unwrap();
    // Resolution survives loss of the current observation, and reaches only the nonces
    // below the watermark, not the next signed payload.
    assert!(stale.nonce_observation().is_none());
    for (hash, state) in [
        (4, ExecutorPayloadState::Resolved),
        (6, ExecutorPayloadState::Resolved),
        (7, ExecutorPayloadState::Pending),
    ] {
        assert_eq!(stale.payload_state(B256::repeat_byte(hash)), Some(state));
    }
    assert_eq!(stale.reserved_inputs(), inputs);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn notes_at_a_resolved_nonce_stay_reserved_until_private_sync_settles_it() {
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
    store
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
    let note = |position: u64| {
        Utxo::new(
            broadcaster_core::notes::Note::new_change(
                U256::ONE,
                Address::ZERO,
                U256::from(9),
                [7; 16],
            ),
            2,
            position,
            UtxoSource {
                tx_hash: B256::repeat_byte(9),
                block_number: 1,
                block_timestamp: 1,
            },
            UtxoCommitmentKind::Transact,
        )
    };
    let notes = [note(3), note(4), note(5)];
    let [setup, variant, order] = notes.each_ref().map(ExecutorInputIdentity::from_utxo);
    let read = |number: u8, nonce: u64| {
        let observed = ExecutorNonceObservation::new(
            BlockNumHash::new(number.into(), B256::repeat_byte(number)),
            U256::from(nonce),
        );
        store.record_account_read(operation, observed).unwrap();
        observed
    };
    let issue = |hash: u8, observed: ExecutorNonceObservation, input: &ExecutorInputIdentity| {
        store
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    observed.nonce(),
                    delegate,
                    B256::repeat_byte(hash),
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from(vec![hash]),
                        observed,
                        vec![input.clone()],
                    ),
                ),
            )
            .unwrap()
    };
    let synced = |last_scanned: u64| store.record_synced(operation, None, last_scanned).unwrap();
    let owner = crate::ExecutorOwner::new(
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
    let own_candidates = |record: &ExecutorRecord| {
        owner
            .inputs_for_record(notes.to_vec(), record)
            .unwrap()
            .iter()
            .map(|input| input.position)
            .collect::<Vec<_>>()
    };

    // Two fee rounds of a setup at nonce 0, then a read at block 20 that shows it consumed.
    let signed = read(10, 0);
    issue(4, signed, &setup);
    issue(5, signed, &variant);
    let consumed = read(20, 1);
    // Private sync is behind block 20: whichever round ran, both keep their notes, and the
    // account's own next payload cannot select them.
    let behind = synced(19);
    assert_eq!(behind.settled_nonce(), None);
    assert_eq!(behind.reserved_inputs(), [setup, variant]);
    assert_eq!(own_candidates(&behind), [5]);

    // Private sync reaches block 20. Its spent status is now the only record of those notes.
    let settled = synced(20);
    assert_eq!(settled.settled_nonce(), Some(U256::ONE));
    assert!(settled.reserved_inputs().is_empty());
    assert_eq!(own_candidates(&settled), [3, 4, 5]);
    // A reset of private sync leaves the settled nonce settled.
    assert_eq!(synced(0), settled);

    // A later nonce is consumed at a block private sync has not reached. Only its notes wait.
    issue(6, consumed, &order);
    read(30, 2);
    let reset = synced(5);
    assert_eq!(reset.settled_nonce(), Some(U256::ONE));
    assert_eq!(reset.reserved_inputs(), std::slice::from_ref(&order));
    assert_eq!(own_candidates(&reset), [3, 4]);
    let rescanned = synced(30);
    assert_eq!(rescanned.settled_nonce(), Some(U256::from(2)));
    assert!(rescanned.reserved_inputs().is_empty());

    // A read that shows nonce 1 unconsumed again lowers the settled nonce with the watermark.
    read(31, 1);
    let lowered = synced(31);
    assert_eq!(lowered.settled_nonce(), Some(U256::ONE));
    assert_eq!(lowered.reserved_inputs(), [order]);
    assert_eq!(own_candidates(&lowered), [3, 4, 5]);
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn record_stored_with_inclusions_loads_and_only_an_executed_one_resolves_its_nonce() {
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::from(3));
    let included = BlockNumHash::new(11, B256::repeat_byte(11));
    // Payloads as an earlier build stored them after its block scan: each with the
    // inclusion it found, one of them still carrying a key that no longer has a field.
    let stored_payload = |nonce: u64, hash: u8, inclusion: serde_json::Value| {
        let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
            U256::from(nonce),
            Address::repeat_byte(1),
            B256::repeat_byte(hash),
            ExecutorPayloadPurpose::Operation,
            ExecutorPayloadContext::new(Bytes::from_static(b"call"), observed, Vec::new()),
        ))
        .unwrap();
        payload["inclusion"] = inclusion;
        payload
    };
    let executed = stored_payload(
        3,
        4,
        serde_json::json!({
            "block": included,
            "transaction_hash": B256::repeat_byte(5),
            "result": "Executed",
            "executor_account_nonce": 7,
        }),
    );
    let reverted = stored_payload(
        4,
        6,
        serde_json::json!({
            "block": BlockNumHash::new(12, B256::repeat_byte(12)),
            "transaction_hash": B256::repeat_byte(7),
            "result": "Reverted",
        }),
    );
    // Background confirmation cleared the nonce observation when it stored an inclusion,
    // and no earlier build stored a watermark. Earlier builds also wrote two recovery keys
    // that no longer have fields.
    let record: ExecutorRecord = serde_json::from_value(serde_json::json!({
        "version": 1,
        "derivation": "Railgun7702V1",
        "origin": "Reserved",
        "operation": ExecutorOperationId::random().unwrap(),
        "index": 0,
        "address": Address::repeat_byte(2),
        "delegate": Address::repeat_byte(1),
        "retired": false,
        "created_at": null,
        "restored_at": null,
        "purpose_summary": null,
        "issued": [executed, reverted],
        "recovery_transactions": [],
        "recovery_observation": null,
    }))
    .expect("a record stored with inclusions remains readable");
    assert!(record.nonce_observation().is_none());
    // The executed inclusion places the watermark with no chain read. The reverted one
    // resolves nothing: its nonce is unconsumed and its signature can still execute.
    assert_eq!(
        record.nonce_watermark(),
        Some(ExecutorNonceWatermark::new(U256::from(4), included.number))
    );
    assert_eq!(
        record.payload_state(B256::repeat_byte(4)),
        Some(ExecutorPayloadState::Resolved)
    );
    assert_eq!(
        record.payload_state(B256::repeat_byte(6)),
        Some(ExecutorPayloadState::Pending)
    );
    assert!(record.has_unresolved_issued_work());
    // Reading the effective value stores nothing: the record keeps its earlier form.
    let stored = serde_json::to_value(&record).unwrap();
    assert!(stored.get("nonce_watermark").is_none());
    // The next write drops the recovery keys.
    assert!(stored.get("recovery_transactions").is_none());
    assert!(stored.get("recovery_observation").is_none());
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<ExecutorRecord>(stored.clone()).unwrap())
            .unwrap(),
        stored
    );

    // A legacy record can also retain a read above its executed inclusion. Invalidation
    // must keep that read's resolution after private sync has settled its payloads.
    let later =
        ExecutorNonceObservation::new(BlockNumHash::new(20, B256::repeat_byte(20)), U256::from(7));
    let inputs = [5, 7].map(|position| {
        ExecutorInputIdentity::from_utxo(&Utxo::new(
            broadcaster_core::notes::Note::new_unshield(Address::ZERO, Address::ZERO, U256::ONE),
            2,
            position,
            UtxoSource {
                tx_hash: B256::repeat_byte(9),
                block_number: 1,
                block_timestamp: 1,
            },
            UtxoCommitmentKind::Transact,
        ))
    });
    let mut legacy = stored;
    legacy["nonce_observation"] = serde_json::to_value(later).unwrap();
    for (nonce, input) in [5, 7].into_iter().zip(&inputs) {
        legacy["issued"].as_array_mut().unwrap().push(
            serde_json::to_value(IssuedExecutorPayload::new(
                U256::from(nonce),
                record.delegate(),
                B256::repeat_byte(nonce),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"call"),
                    observed,
                    vec![input.clone()],
                ),
            ))
            .unwrap(),
        );
    }
    let legacy: ExecutorRecord = serde_json::from_value(legacy).unwrap();
    let watermark = Some(ExecutorNonceWatermark::from(later));
    assert_eq!(legacy.nonce_watermark(), watermark);
    assert!(
        serde_json::to_value(&legacy)
            .unwrap()
            .get("nonce_watermark")
            .is_none()
    );

    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let operation = legacy.operation();
    let key = format!(
        "{}{}",
        super::super::executors::executor_operation_prefix(view.wallet_id(), 1),
        operation.opaque_id()
    );
    let identity = format!("{}:{}:1:{key}", view.wallet_id().len(), view.wallet_id());
    let encrypted = view
        .private_view
        .encrypt_record(
            RecordKind::ExecutorOperation,
            &identity,
            &rmp_serde::to_vec_named(&legacy).unwrap(),
        )
        .unwrap()
        .to_record_entry(key)
        .unwrap();
    db.put_desktop_wallet_vault_records(&[encrypted]).unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let settled = store
        .record_synced(operation, None, later.block().number)
        .unwrap();
    assert_eq!(settled.settled_nonce(), Some(U256::from(7)));
    assert_eq!(
        settled.payload_state(B256::repeat_byte(5)),
        Some(ExecutorPayloadState::Resolved)
    );
    assert_eq!(
        settled.payload_state(B256::repeat_byte(7)),
        Some(ExecutorPayloadState::Pending)
    );
    assert_eq!(settled.reserved_inputs(), std::slice::from_ref(&inputs[1]));
    store.invalidate_observation(operation).unwrap();
    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let reloaded = store.records().unwrap().remove(0);
    assert!(reloaded.nonce_observation().is_none());
    assert_eq!(reloaded.nonce_watermark(), watermark);
    assert_eq!(reloaded.settled_nonce(), settled.settled_nonce());
    for payload in settled.issued() {
        assert_eq!(
            reloaded.payload_state(payload.hash()),
            settled.payload_state(payload.hash())
        );
    }
    assert_eq!(reloaded.reserved_inputs(), settled.reserved_inputs());

    let lowered = store
        .record_account_read(
            operation,
            ExecutorNonceObservation::new(
                BlockNumHash::new(21, B256::repeat_byte(21)),
                U256::from(5),
            ),
        )
        .unwrap();
    assert_eq!(
        lowered.nonce_watermark(),
        Some(ExecutorNonceWatermark::new(U256::from(5), 21))
    );
    assert_eq!(lowered.settled_nonce(), Some(U256::from(5)));
    assert_eq!(
        lowered.payload_state(B256::repeat_byte(5)),
        Some(ExecutorPayloadState::Pending)
    );
    assert_eq!(lowered.reserved_inputs(), inputs);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn released_executor_inputs_stay_spendable_until_a_later_payload_reserves_them() {
    use crate::{ExecutorInputLockReason, ExecutorOwner, HttpContext};

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
    store
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
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
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::from(3));
    let issue = |hash: u8| {
        store.record_account_read(operation, observed).unwrap();
        store
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    U256::from(3),
                    delegate,
                    B256::repeat_byte(hash),
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from(vec![hash]),
                        observed,
                        vec![ExecutorInputIdentity::from_utxo(&input)],
                    ),
                ),
            )
            .unwrap();
    };
    let chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let owner = |generation| {
        ExecutorOwner::new(
            generation,
            db.clone(),
            view.clone(),
            chain.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap()
    };
    issue(4);

    // A saved nonce observation cannot resolve a never-included payload, even after restart.
    let restarted = owner(0);
    assert!(
        restarted
            .available_inputs(vec![input.clone()])
            .unwrap()
            .is_empty()
    );
    let locks = restarted.input_locks(std::slice::from_ref(&input)).unwrap();
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].operation(), operation);
    assert_eq!(
        locks[0].reason(),
        ExecutorInputLockReason::SignedNotConfirmed
    );
    assert_eq!(locks[0].notes().count(), 1);
    restarted.release_input_lock(operation).unwrap();
    assert_eq!(
        restarted
            .available_inputs(vec![input.clone()])
            .unwrap()
            .len(),
        1
    );
    assert!(
        restarted
            .input_locks(std::slice::from_ref(&input))
            .unwrap()
            .is_empty()
    );
    drop(restarted);

    // The release is durable.
    let reloaded = owner(1);
    assert_eq!(
        reloaded
            .available_inputs(vec![input.clone()])
            .unwrap()
            .len(),
        1
    );
    drop(reloaded);

    // Sending the released payload again reserves its note again.
    store
        .record_submission(operation, B256::repeat_byte(4), B256::repeat_byte(40))
        .unwrap();
    let resent = owner(2);
    assert!(
        resent
            .available_inputs(vec![input.clone()])
            .unwrap()
            .is_empty()
    );
    resent.release_input_lock(operation).unwrap();
    assert_eq!(
        resent.available_inputs(vec![input.clone()]).unwrap().len(),
        1
    );
    drop(resent);

    // A payload issued after the release reserves the note again.
    issue(5);
    let reissued = owner(3);
    assert!(reissued.available_inputs(vec![input]).unwrap().is_empty());
    drop(reissued);
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
