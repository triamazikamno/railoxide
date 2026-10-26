use super::super::*;
use super::helpers::*;
use alloy::primitives::{FixedBytes, U256};
use alloy::uint;
use local_db::{
    DbError, OpaqueWalletPrivateRow, OutputPoiRecoveryRecord, OutputPoiRecoveryStatus,
    PendingOutputPoiContextRecord, PendingOutputPoiObservation, PendingOutputPoiRole,
    WalletPrivateNamespaceId, WalletPrivateRecordKind,
};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::fs;
use std::path::Path;

type RawWalletPrivateTable = TableDefinition<'static, &'static str, &'static [u8]>;

const PENDING_OUTPUT_CONTEXT_V1: RawWalletPrivateTable =
    TableDefinition::new("pending_output_poi_context");
const OUTPUT_POI_RECOVERY_V1: RawWalletPrivateTable = TableDefinition::new("output_poi_recovery");
const PENDING_OUTPUT_CONTEXT_V2: RawWalletPrivateTable =
    TableDefinition::new("pending_output_poi_context_v2");
const OUTPUT_POI_RECOVERY_V2: RawWalletPrivateTable =
    TableDefinition::new("output_poi_recovery_v2");
const SENDER_TRANSACTION_CANDIDATE_V1: RawWalletPrivateTable =
    TableDefinition::new("sender_transaction_candidate_v1");

fn put_raw_wallet_private_row(
    db_path: &Path,
    definition: RawWalletPrivateTable,
    key: &str,
    payload: &[u8],
) {
    let db = Database::open(db_path).expect("open raw wallet database");
    let txn = db.begin_write().expect("begin raw wallet write");
    {
        let mut table = txn.open_table(definition).expect("open raw wallet table");
        table.insert(key, payload).expect("insert raw wallet row");
    }
    txn.commit().expect("commit raw wallet row");
}

fn raw_wallet_private_rows(
    db_path: &Path,
    definition: RawWalletPrivateTable,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let db = Database::open(db_path).expect("open raw wallet database");
    let txn = db.begin_read().expect("begin raw wallet read");
    let table = txn.open_table(definition).expect("open raw wallet table");
    table
        .iter()
        .expect("iterate raw wallet rows")
        .map(|entry| {
            let (key, payload) = entry.expect("read raw wallet row");
            (key.value().as_bytes().to_vec(), payload.value().to_vec())
        })
        .collect()
}

fn sample_private_workflow_records(
    chain_id: u64,
    wallet_id: &str,
    marker: u8,
) -> (PendingOutputPoiContextRecord, OutputPoiRecoveryRecord) {
    let output_commitment = FixedBytes::from([marker; KEY_LEN]);
    (
        PendingOutputPoiContextRecord {
            chain_id,
            wallet_id: wallet_id.to_owned(),
            txid_version: "V2_PoseidonMerkle".to_string(),
            output_commitment,
            output_npk: FixedBytes::from([marker.wrapping_add(1); KEY_LEN]),
            utxo_tree_in: 7,
            railgun_txid: U256::from_be_bytes([marker.wrapping_add(2); KEY_LEN]),
            txid_merkleroot_index: Some(9),
            pre_transaction_pois_per_txid_leaf_per_list: BTreeMap::new(),
            required_poi_list_keys: vec![FixedBytes::from([marker.wrapping_add(3); KEY_LEN])],
            output_role: PendingOutputPoiRole::Recipient,
            created_at: 10,
            source_operation_id: Some("recipient-role-sentinel".to_string()),
            observation: Some(PendingOutputPoiObservation {
                output_tree: 8,
                output_position: 11,
                tx_hash: FixedBytes::from([marker.wrapping_add(4); KEY_LEN]),
                block_number: 12,
                block_timestamp: 13,
            }),
            submitted_poi_list_keys: Vec::new(),
            terminal_error: None,
        },
        OutputPoiRecoveryRecord {
            chain_id,
            wallet_id: wallet_id.to_owned(),
            output_commitment,
            source_tx_hash: FixedBytes::from([marker.wrapping_add(5); KEY_LEN]),
            tx_input: Some(vec![marker.wrapping_add(6); KEY_LEN]),
            status: OutputPoiRecoveryStatus::Recoverable,
            created_at: 14,
            updated_at: 15,
            last_detection_at: Some(16),
            last_submission_at: None,
            next_retry_at: Some(17),
            attempt_count: 2,
            last_error: Some("recovery-role-sentinel".to_string()),
        },
    )
}

fn sample_sender_transaction_candidate(
    chain_id: u64,
    wallet_id: &WalletCacheKey,
    transaction_marker: u8,
    note_marker: u8,
    block_number: u64,
) -> sync_service::SenderTransactionCandidate {
    use railgun_wallet::{Note, UtxoSource};
    use sync_service::{SenderTransactionCandidateOutput, SenderTransactionCandidateSpend};

    let note = Note {
        token_hash: U256::from_be_bytes([note_marker; KEY_LEN]),
        value: U256::from(note_marker),
        random: [note_marker; 16],
        npk: U256::from_be_bytes([note_marker.wrapping_add(1); KEY_LEN]),
    };
    let output_commitment = FixedBytes::from(note.commitment().to_be_bytes::<KEY_LEN>());
    sync_service::SenderTransactionCandidate::new(
        chain_id,
        wallet_id.clone(),
        UtxoSource {
            tx_hash: FixedBytes::from([transaction_marker; KEY_LEN]),
            block_number,
            block_timestamp: 1_700_000_000 + block_number,
        },
        vec![SenderTransactionCandidateSpend {
            tree: 1,
            position: 2,
            commitment: FixedBytes::from([transaction_marker.wrapping_add(1); KEY_LEN]),
        }],
        vec![SenderTransactionCandidateOutput {
            tree: 3,
            position: 4,
            commitment: output_commitment,
            note: Some(note),
        }],
    )
    .expect("valid sender transaction candidate")
}

fn encrypted_private_workflow_row<T: Serialize>(
    cache_keys: &CacheKeys,
    namespace: &WalletPrivateNamespaceId,
    kind: WalletPrivateRecordKind,
    output_commitment: &FixedBytes<KEY_LEN>,
    record: &T,
) -> OpaqueWalletPrivateRow {
    let row_id = cache_keys.private_row_id(
        kind,
        namespace.wallet_id.as_str(),
        output_commitment.as_slice(),
    );
    let plaintext = rmp_serde::to_vec_named(record).expect("encode private workflow row");
    let encrypted = cache_keys
        .encrypt_private_row(kind, namespace.wallet_id.as_str(), &row_id, &plaintext)
        .expect("encrypt private workflow row");
    OpaqueWalletPrivateRow {
        row_id: row_id.to_vec(),
        payload: rmp_serde::to_vec_named(&encrypted).expect("encode encrypted workflow row"),
    }
}

fn assert_private_sentinels_absent(rows: &[(Vec<u8>, Vec<u8>)], marker: u8) {
    for (key, payload) in rows {
        for sentinel in marker..=marker.wrapping_add(6) {
            assert!(!contains_subsequence(key, &[sentinel; KEY_LEN]));
            assert!(!contains_subsequence(payload, &[sentinel; KEY_LEN]));
        }
        assert!(!contains_subsequence(key, b"recipient-role-sentinel"));
        assert!(!contains_subsequence(payload, b"recipient-role-sentinel"));
        assert!(!contains_subsequence(key, b"recovery-role-sentinel"));
        assert!(!contains_subsequence(payload, b"recovery-role-sentinel"));
        assert!(!contains_subsequence(key, b"Recipient"));
        assert!(!contains_subsequence(payload, b"Recipient"));
    }
}

#[test]
fn cache_row_ids_are_deterministic_and_context_bound() {
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    let cache_keys = created
        .view
        .derive_cache_keys("opaque-wallet-chain-a")
        .expect("cache keys");
    let other_cache_keys = created
        .view
        .derive_cache_keys("opaque-wallet-chain-b")
        .expect("other cache keys");

    let row_id = cache_keys.row_id(4, 42, b"stable-utxo");
    let same_row_id = cache_keys.row_id(4, 42, b"stable-utxo");
    let other_position = cache_keys.row_id(4, 43, b"stable-utxo");
    let other_namespace = other_cache_keys.row_id(4, 42, b"stable-utxo");

    assert_eq!(row_id, same_row_id);
    assert_ne!(row_id, other_position);
    assert_ne!(row_id, other_namespace);
    assert_eq!(CacheKeys::row_record_id(&row_id).len(), 64);
}
#[test]
fn encrypted_cache_rows_are_bound_to_opaque_row_id() {
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    let cache_keys = created
        .view
        .derive_cache_keys("opaque-wallet-chain")
        .expect("cache keys");
    let row_id = cache_keys.row_id(4, 42, b"stable-utxo");
    let other_row_id = cache_keys.row_id(4, 43, b"stable-utxo");
    let record = cache_keys
        .encrypt_row(&row_id, b"private utxo payload")
        .expect("encrypt row");
    let mut tampered = record.clone();
    tampered.ciphertext[0] ^= 0x01;

    let plaintext = cache_keys
        .decrypt_row(&row_id, &record)
        .expect("decrypt row");
    assert_eq!(&*plaintext, b"private utxo payload");
    assert!(cache_keys.decrypt_row(&other_row_id, &record).is_err());
    assert!(cache_keys.decrypt_row(&row_id, &tampered).is_err());
}

#[test]
fn private_workflow_rows_are_kind_namespace_and_row_bound() {
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    let cache_keys = created
        .view
        .derive_cache_keys("opaque-wallet-chain")
        .expect("cache keys");
    let semantic_id = [0x42; KEY_LEN];
    let pending_row_id = cache_keys.private_row_id(
        WalletPrivateRecordKind::PendingOutputPoiContext,
        "opaque-wallet-chain",
        &semantic_id,
    );
    let recovery_row_id = cache_keys.private_row_id(
        WalletPrivateRecordKind::OutputPoiRecovery,
        "opaque-wallet-chain",
        &semantic_id,
    );
    let other_namespace_row_id = cache_keys.private_row_id(
        WalletPrivateRecordKind::PendingOutputPoiContext,
        "other-wallet-chain",
        &semantic_id,
    );
    assert_ne!(pending_row_id, recovery_row_id);
    assert_ne!(pending_row_id, other_namespace_row_id);

    let record = cache_keys
        .encrypt_private_row(
            WalletPrivateRecordKind::PendingOutputPoiContext,
            "opaque-wallet-chain",
            &pending_row_id,
            b"private workflow payload",
        )
        .expect("encrypt private workflow row");
    assert!(
        cache_keys
            .decrypt_private_row(
                WalletPrivateRecordKind::OutputPoiRecovery,
                "opaque-wallet-chain",
                &pending_row_id,
                &record,
            )
            .is_err()
    );
    assert!(
        cache_keys
            .decrypt_private_row(
                WalletPrivateRecordKind::PendingOutputPoiContext,
                "other-wallet-chain",
                &pending_row_id,
                &record,
            )
            .is_err()
    );
    assert!(
        cache_keys
            .decrypt_private_row(
                WalletPrivateRecordKind::PendingOutputPoiContext,
                "opaque-wallet-chain",
                &other_namespace_row_id,
                &record,
            )
            .is_err()
    );
}

#[test]
fn current_poi_workflow_writes_are_opaque_and_restart_safe() {
    use sync_service::WalletCacheStore;

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let db_path = db.db_path();
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "current-private-workflow-wallet";
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, wallet_id)
            .expect("load view session"),
    );
    let chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let wallet_cache_key = chain_metadata
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("wallet cache key");
    let cache_store = DesktopEncryptedWalletCacheStore::new(
        Arc::clone(&db),
        &view_session,
        chain_metadata.clone(),
    )
    .expect("encrypted cache store");
    let mismatch = cache_store
        .commit_poi_workflow_for_test(2, &wallet_cache_key, &[], &[])
        .expect_err("reject mismatched wallet-private chain namespace");
    assert!(matches!(
        mismatch,
        WalletCacheError::Db(DbError::InvalidWalletPrivateCommitNamespace {
            expected_chain_id: 1,
            actual_chain_id: 2,
            ..
        })
    ));
    let (retained_pending, retained_recovery) =
        sample_private_workflow_records(1, wallet_cache_key.as_str(), 0x51);
    let (deleted_pending, deleted_recovery) =
        sample_private_workflow_records(1, wallet_cache_key.as_str(), 0x61);

    cache_store
        .commit_poi_workflow_for_test(
            1,
            &wallet_cache_key,
            &[retained_pending.clone(), deleted_pending.clone()],
            &[retained_recovery.clone(), deleted_recovery.clone()],
        )
        .expect("commit encrypted POI workflow rows");
    assert_eq!(
        cache_store
            .get_pending_output_poi_context(
                1,
                &wallet_cache_key,
                &retained_pending.output_commitment,
            )
            .expect("read encrypted pending output")
            .expect("pending output present")
            .output_npk,
        retained_pending.output_npk
    );
    assert_eq!(
        cache_store
            .get_output_poi_recovery(1, &wallet_cache_key, &retained_recovery.output_commitment,)
            .expect("read encrypted output recovery")
            .expect("output recovery present")
            .source_tx_hash,
        retained_recovery.source_tx_hash
    );
    cache_store
        .delete_poi_workflow_for_test(
            1,
            &wallet_cache_key,
            &[deleted_pending.output_commitment],
            &[deleted_recovery.output_commitment],
        )
        .expect("delete encrypted POI workflow rows");
    assert!(
        cache_store
            .get_pending_output_poi_context(
                1,
                &wallet_cache_key,
                &deleted_pending.output_commitment,
            )
            .expect("read deleted pending output")
            .is_none()
    );

    drop(cache_store);
    drop(store);
    drop(db);
    let pending_rows = raw_wallet_private_rows(&db_path, PENDING_OUTPUT_CONTEXT_V2);
    let recovery_rows = raw_wallet_private_rows(&db_path, OUTPUT_POI_RECOVERY_V2);
    assert_eq!(pending_rows.len(), 1);
    assert_eq!(recovery_rows.len(), 1);
    assert_private_sentinels_absent(&pending_rows, 0x51);
    assert_private_sentinels_absent(&recovery_rows, 0x51);

    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("reopen db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let cache_store =
        DesktopEncryptedWalletCacheStore::new(Arc::clone(&db), &view_session, chain_metadata)
            .expect("reopen encrypted cache store");
    assert_eq!(
        cache_store
            .list_pending_output_poi_contexts(1, &wallet_cache_key)
            .expect("list pending outputs after restart")
            .len(),
        1
    );
    assert_eq!(
        cache_store
            .list_output_poi_recoveries(1, &wallet_cache_key)
            .expect("list recoveries after restart")
            .len(),
        1
    );

    drop(cache_store);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn sender_transaction_candidates_are_opaque_restart_safe_and_fail_closed() {
    use sync_service::WalletCacheStore;

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let db_path = db.db_path();
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "sender-candidate-wallet";
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, wallet_id)
            .expect("load view session"),
    );
    let chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let wallet_cache_key = chain_metadata
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("wallet cache key");
    let namespace = WalletPrivateNamespaceId::new(1, wallet_cache_key.clone());
    let cache_store = DesktopEncryptedWalletCacheStore::new(
        Arc::clone(&db),
        &view_session,
        chain_metadata.clone(),
    )
    .expect("encrypted cache store");
    let candidate = sample_sender_transaction_candidate(1, &wallet_cache_key, 0xa1, 0xb2, 120);

    cache_store
        .commit_sender_transaction_candidates_for_test(
            1,
            &wallet_cache_key,
            std::slice::from_ref(&candidate),
            &[],
        )
        .expect("persist sender candidate");
    let loaded = cache_store
        .get_sender_transaction_candidate(1, &wallet_cache_key, &candidate.semantic_id())
        .expect("load sender candidate")
        .expect("sender candidate present");
    assert_eq!(
        loaded.encode().expect("encode loaded candidate"),
        candidate.encode().unwrap()
    );

    let opaque_row = db
        .list_opaque_wallet_private_rows(
            &namespace,
            WalletPrivateRecordKind::SenderTransactionCandidate,
        )
        .expect("list opaque candidate rows")
        .pop()
        .expect("opaque candidate row");
    let encrypted: EncryptedRecord =
        rmp_serde::from_slice(&opaque_row.payload).expect("decode encrypted envelope");
    let row_id: [u8; KEY_LEN] = opaque_row.row_id.clone().try_into().expect("opaque row id");
    assert!(
        cache_store
            .cache_keys
            .decrypt_private_row(
                WalletPrivateRecordKind::OutputPoiRecovery,
                namespace.wallet_id.as_str(),
                &row_id,
                &encrypted,
            )
            .is_err()
    );
    let mut wrong_row = opaque_row.clone();
    wrong_row.row_id[0] ^= 1;
    assert!(
        cache_store
            .decode_sender_transaction_candidate_row(&namespace, &wrong_row)
            .is_err()
    );
    let other_namespace =
        WalletPrivateNamespaceId::new(1, WalletCacheKey::from_opaque_id([0x44; 16]));
    assert!(
        cache_store
            .decode_sender_transaction_candidate_row(&other_namespace, &opaque_row)
            .is_err()
    );

    let wrong_semantic_id = FixedBytes::from([0xc3; KEY_LEN]);
    let wrong_semantic_row_id = cache_store.cache_keys.private_row_id(
        WalletPrivateRecordKind::SenderTransactionCandidate,
        namespace.wallet_id.as_str(),
        wrong_semantic_id.as_slice(),
    );
    let wrong_semantic_envelope = cache_store
        .cache_keys
        .encrypt_private_row(
            WalletPrivateRecordKind::SenderTransactionCandidate,
            namespace.wallet_id.as_str(),
            &wrong_semantic_row_id,
            &candidate.encode().expect("encode candidate"),
        )
        .expect("encrypt semantically mismatched candidate");
    assert!(
        cache_store
            .decode_sender_transaction_candidate_row(
                &namespace,
                &OpaqueWalletPrivateRow {
                    row_id: wrong_semantic_row_id.to_vec(),
                    payload: rmp_serde::to_vec_named(&wrong_semantic_envelope)
                        .expect("encode mismatched envelope"),
                },
            )
            .is_err()
    );

    drop(cache_store);
    drop(store);
    drop(db);
    let raw_rows = raw_wallet_private_rows(&db_path, SENDER_TRANSACTION_CANDIDATE_V1);
    assert_eq!(raw_rows.len(), 1);
    for (key, payload) in &raw_rows {
        assert!(!contains_subsequence(
            key,
            candidate.semantic_id().as_slice()
        ));
        assert!(!contains_subsequence(
            payload,
            candidate.semantic_id().as_slice()
        ));
        assert!(!contains_subsequence(key, &[0xb2; 16]));
        assert!(!contains_subsequence(payload, &[0xb2; 16]));
        assert!(sync_service::SenderTransactionCandidate::decode(payload).is_err());
    }
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("reopen db"),
    );
    let cache_store =
        DesktopEncryptedWalletCacheStore::new(Arc::clone(&db), &view_session, chain_metadata)
            .expect("reopen encrypted cache store");
    assert_eq!(
        cache_store
            .list_sender_transaction_candidates(1, &wallet_cache_key)
            .expect("list candidates after restart")
            .len(),
        1
    );

    drop(cache_store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn chain_cache_repair_rewinds_sender_candidates_at_the_boundary() {
    use sync_service::WalletCacheStore;

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "sender-candidate-rewind-wallet";
    store
        .import_wallet_mnemonic(
            TEST_PASSWORD,
            wallet_id,
            0,
            "english",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        )
        .expect("import wallet");
    let view_session = Arc::new(store.load_view_session(TEST_PASSWORD, wallet_id).unwrap());
    let mut chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .unwrap();
    let wallet_cache_key = chain_metadata.wallet_chain_uuid.parse().unwrap();
    let cache_store = DesktopEncryptedWalletCacheStore::new(
        Arc::clone(&db),
        &view_session,
        chain_metadata.clone(),
    )
    .unwrap();
    let retained = sample_sender_transaction_candidate(1, &wallet_cache_key, 0x21, 0x31, 149);
    let boundary = sample_sender_transaction_candidate(1, &wallet_cache_key, 0x22, 0x32, 150);
    let after_boundary = sample_sender_transaction_candidate(1, &wallet_cache_key, 0x23, 0x33, 160);
    cache_store
        .commit_sender_transaction_candidates_for_test(
            1,
            &wallet_cache_key,
            &[retained.clone(), boundary, after_boundary],
            &[],
        )
        .unwrap();
    store
        .rewind_wallet_chain_cache_with_session(view_session.as_ref(), &mut chain_metadata, 150)
        .expect("rewind chain cache and sender candidates");

    let remaining = cache_store
        .list_sender_transaction_candidates(1, &wallet_cache_key)
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].semantic_id(), retained.semantic_id());

    store
        .reset_wallet_chain_cache_with_session(view_session.as_ref(), &mut chain_metadata, 100)
        .expect("reset chain cache to deployment");
    assert!(
        cache_store
            .list_sender_transaction_candidates(1, &wallet_cache_key)
            .expect("list candidates after deployment reset")
            .is_empty()
    );

    drop(cache_store);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn malformed_sender_candidate_aborts_chain_cache_repair_atomically() {
    use local_db::WalletSyncActorStateRecord;
    use railgun_wallet::{Note, Utxo, UtxoCommitmentKind, UtxoSource};

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "sender-candidate-repair-failure-wallet";
    store
        .import_wallet_mnemonic(
            TEST_PASSWORD,
            wallet_id,
            0,
            "english",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        )
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, wallet_id)
            .expect("load view session"),
    );
    let mut chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let wallet_cache_key = chain_metadata
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("wallet cache key");
    let namespace = WalletPrivateNamespaceId::new(1, wallet_cache_key.clone());
    let cache_store = DesktopEncryptedWalletCacheStore::new(
        Arc::clone(&db),
        &view_session,
        chain_metadata.clone(),
    )
    .expect("encrypted cache store");
    let utxo = WalletUtxo {
        utxo: Utxo::new(
            Note {
                token_hash: U256::from_be_bytes([0x11; KEY_LEN]),
                value: uint!(1_U256),
                random: [0x22; 16],
                npk: U256::from_be_bytes([0x33; KEY_LEN]),
            },
            3,
            1,
            UtxoSource {
                tx_hash: FixedBytes::from([0x44; KEY_LEN]),
                block_number: 120,
                block_timestamp: 1_700_000_120,
            },
            UtxoCommitmentKind::Transact,
        ),
        spent: None,
    };
    cache_store
        .replace_wallet_cache_atomically_for_test(
            &wallet_cache_key,
            std::slice::from_ref(&utxo),
            180,
            Some([0xaa; KEY_LEN]),
        )
        .expect("seed wallet cache");
    let candidate = sample_sender_transaction_candidate(1, &wallet_cache_key, 0x51, 0x61, 160);
    cache_store
        .commit_sender_transaction_candidates_for_test(
            1,
            &wallet_cache_key,
            std::slice::from_ref(&candidate),
            &[],
        )
        .expect("seed candidate");
    let valid_row = db
        .list_opaque_wallet_private_rows(
            &namespace,
            WalletPrivateRecordKind::SenderTransactionCandidate,
        )
        .expect("list candidate rows")
        .pop()
        .expect("candidate row");
    db.put_opaque_wallet_private_row(
        &namespace,
        WalletPrivateRecordKind::SenderTransactionCandidate,
        &OpaqueWalletPrivateRow {
            row_id: vec![0xef; KEY_LEN],
            payload: valid_row.payload,
        },
    )
    .expect("inject malformed encrypted candidate row");
    db.put_wallet_sync_actor_state(&WalletSyncActorStateRecord {
        chain_id: 1,
        wallet_id: wallet_cache_key.to_string(),
        highest_accepted_reset_intent: 7,
        pending_reset: None,
        updated_at: 1,
    })
    .expect("seed actor state");

    let utxos_before = db.list_wallet_utxos(&wallet_cache_key).unwrap();
    let wallet_meta_before =
        rmp_serde::to_vec_named(&db.get_wallet_meta(&wallet_cache_key).unwrap())
            .expect("encode wallet meta snapshot");
    let actor_before = db
        .get_wallet_sync_actor_state(1, wallet_cache_key.as_str())
        .unwrap();
    let vault_before = db
        .get_desktop_wallet_vault_record(&wallet_chain_metadata_record_key(
            &chain_metadata.wallet_chain_uuid,
        ))
        .unwrap();
    let candidates_before = db
        .list_opaque_wallet_private_rows(
            &namespace,
            WalletPrivateRecordKind::SenderTransactionCandidate,
        )
        .unwrap();
    let in_memory_metadata_before =
        rmp_serde::to_vec_named(&chain_metadata).expect("encode metadata snapshot");

    assert!(
        store
            .rewind_wallet_chain_cache_with_session(
                view_session.as_ref(),
                &mut chain_metadata,
                150,
            )
            .is_err()
    );

    assert_eq!(
        db.list_wallet_utxos(&wallet_cache_key).unwrap(),
        utxos_before
    );
    assert_eq!(
        rmp_serde::to_vec_named(&db.get_wallet_meta(&wallet_cache_key).unwrap())
            .expect("encode unchanged wallet meta"),
        wallet_meta_before
    );
    assert_eq!(
        db.get_wallet_sync_actor_state(1, wallet_cache_key.as_str())
            .unwrap(),
        actor_before
    );
    assert_eq!(
        db.get_desktop_wallet_vault_record(&wallet_chain_metadata_record_key(
            &chain_metadata.wallet_chain_uuid,
        ))
        .unwrap(),
        vault_before
    );
    assert_eq!(
        db.list_opaque_wallet_private_rows(
            &namespace,
            WalletPrivateRecordKind::SenderTransactionCandidate,
        )
        .unwrap(),
        candidates_before
    );
    assert_eq!(
        rmp_serde::to_vec_named(&chain_metadata).expect("encode unchanged metadata"),
        in_memory_metadata_before
    );

    drop(cache_store);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn shipped_plaintext_poi_rows_migrate_before_reads_and_restart_idempotently() {
    use sync_service::WalletCacheStore;

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let db_path = db.db_path();
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "22".repeat(16);
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, &wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, &wallet_id)
            .expect("load view session"),
    );
    let chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let wallet_cache_key = chain_metadata
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("wallet cache key");
    let (pending, recovery) = sample_private_workflow_records(1, &wallet_id, 0x71);
    let pending_key = format!(
        "1|{wallet_id}|{}",
        alloy::hex::encode(pending.output_commitment)
    );
    let recovery_key = format!(
        "1|{wallet_id}|{}",
        alloy::hex::encode(recovery.output_commitment)
    );
    let pending_payload = rmp_serde::to_vec_named(&pending).expect("encode shipped pending row");
    let recovery_payload = rmp_serde::to_vec_named(&recovery).expect("encode shipped recovery row");
    drop(store);
    drop(db);
    put_raw_wallet_private_row(
        &db_path,
        PENDING_OUTPUT_CONTEXT_V1,
        &pending_key,
        &pending_payload,
    );
    put_raw_wallet_private_row(
        &db_path,
        OUTPUT_POI_RECOVERY_V1,
        &recovery_key,
        &recovery_payload,
    );

    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("reopen db for migration"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let cache_store = DesktopEncryptedWalletCacheStore::new(
        Arc::clone(&db),
        &view_session,
        chain_metadata.clone(),
    )
    .expect("migrate shipped plaintext workflow rows");
    let migrated_pending = cache_store
        .list_pending_output_poi_contexts(1, &wallet_cache_key)
        .expect("list migrated pending outputs");
    let migrated_recovery = cache_store
        .list_output_poi_recoveries(1, &wallet_cache_key)
        .expect("list migrated recoveries");
    assert_eq!(migrated_pending.len(), 1);
    assert_eq!(migrated_pending[0].wallet_id, wallet_cache_key.as_str());
    assert_eq!(migrated_pending[0].output_npk, pending.output_npk);
    assert_eq!(migrated_recovery.len(), 1);
    assert_eq!(migrated_recovery[0].source_tx_hash, recovery.source_tx_hash);

    drop(cache_store);
    drop(store);
    drop(db);
    assert_eq!(
        raw_wallet_private_rows(&db_path, PENDING_OUTPUT_CONTEXT_V1),
        [] as [(Vec<u8>, Vec<u8>); 0]
    );
    assert_eq!(
        raw_wallet_private_rows(&db_path, OUTPUT_POI_RECOVERY_V1),
        [] as [(Vec<u8>, Vec<u8>); 0]
    );
    let pending_rows = raw_wallet_private_rows(&db_path, PENDING_OUTPUT_CONTEXT_V2);
    let recovery_rows = raw_wallet_private_rows(&db_path, OUTPUT_POI_RECOVERY_V2);
    assert_eq!(pending_rows.len(), 1);
    assert_eq!(recovery_rows.len(), 1);
    assert_private_sentinels_absent(&pending_rows, 0x71);
    assert_private_sentinels_absent(&recovery_rows, 0x71);

    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("reopen migrated db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let cache_store =
        DesktopEncryptedWalletCacheStore::new(Arc::clone(&db), &view_session, chain_metadata)
            .expect("repeat idempotent migration");
    assert_eq!(
        cache_store
            .list_pending_output_poi_contexts(1, &wallet_cache_key)
            .expect("list migrated pending outputs after restart")
            .len(),
        1
    );

    drop(cache_store);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn canonical_recovery_wins_over_authenticated_legacy_duplicate() {
    use sync_service::WalletCacheStore;

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "34".repeat(16);
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, &wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, &wallet_id)
            .expect("load view session"),
    );
    let chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let cache_keys = view_session
        .derive_cache_keys(&chain_metadata.wallet_chain_uuid)
        .expect("derive cache keys");
    let canonical_namespace = WalletPrivateNamespaceId::new(
        1,
        chain_metadata
            .wallet_chain_uuid
            .parse()
            .expect("canonical wallet cache key"),
    );
    let legacy_namespace =
        WalletPrivateNamespaceId::new(1, wallet_id.parse().expect("legacy wallet cache key"));
    let (_, mut legacy_recovery) =
        sample_private_workflow_records(1, legacy_namespace.wallet_id.as_str(), 0x42);
    let mut canonical_recovery = legacy_recovery.clone();
    canonical_recovery.wallet_id = canonical_namespace.wallet_id.to_string();
    canonical_recovery.status = OutputPoiRecoveryStatus::Valid;
    canonical_recovery.updated_at = 99;
    canonical_recovery.next_retry_at = None;
    canonical_recovery.last_error = None;
    legacy_recovery.updated_at = 15;
    let canonical_row = encrypted_private_workflow_row(
        &cache_keys,
        &canonical_namespace,
        WalletPrivateRecordKind::OutputPoiRecovery,
        &canonical_recovery.output_commitment,
        &canonical_recovery,
    );
    let legacy_row = encrypted_private_workflow_row(
        &cache_keys,
        &legacy_namespace,
        WalletPrivateRecordKind::OutputPoiRecovery,
        &legacy_recovery.output_commitment,
        &legacy_recovery,
    );
    db.put_opaque_wallet_private_row(
        &canonical_namespace,
        WalletPrivateRecordKind::OutputPoiRecovery,
        &canonical_row,
    )
    .expect("seed canonical recovery");
    db.put_opaque_wallet_private_row(
        &legacy_namespace,
        WalletPrivateRecordKind::OutputPoiRecovery,
        &legacy_row,
    )
    .expect("seed legacy recovery");

    let cache_store = DesktopEncryptedWalletCacheStore::new(
        Arc::clone(&db),
        &view_session,
        chain_metadata.clone(),
    )
    .expect("canonicalize duplicate recovery");
    let loaded = cache_store
        .get_output_poi_recovery(
            1,
            &canonical_namespace.wallet_id,
            &canonical_recovery.output_commitment,
        )
        .expect("read canonical recovery")
        .expect("canonical recovery present");
    assert_eq!(loaded.status, OutputPoiRecoveryStatus::Valid);
    assert_eq!(loaded.updated_at, 99);
    assert_eq!(
        db.list_opaque_wallet_private_rows(
            &canonical_namespace,
            WalletPrivateRecordKind::OutputPoiRecovery,
        )
        .expect("list canonical rows"),
        vec![canonical_row]
    );
    assert_eq!(
        db.list_opaque_wallet_private_rows(
            &legacy_namespace,
            WalletPrivateRecordKind::OutputPoiRecovery,
        )
        .expect("list consumed legacy rows"),
        [] as [OpaqueWalletPrivateRow; 0]
    );
    assert_eq!(
        db.wallet_private_canonicalization_version(&canonical_namespace)
            .expect("read canonicalization version"),
        1
    );
    drop(cache_store);
    DesktopEncryptedWalletCacheStore::new(Arc::clone(&db), &view_session, chain_metadata)
        .expect("restart skips completed migration");

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn shipped_plaintext_poi_migration_conflict_rolls_back() {
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let db_path = db.db_path();
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "33".repeat(16);
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, &wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, &wallet_id)
            .expect("load view session"),
    );
    let chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let cache_keys = view_session
        .derive_cache_keys(&chain_metadata.wallet_chain_uuid)
        .expect("derive cache keys");
    let legacy_wallet_id = wallet_id
        .parse::<WalletCacheKey>()
        .expect("legacy wallet cache key");
    let legacy_namespace = WalletPrivateNamespaceId::new(1, legacy_wallet_id);
    let (pending, _) = sample_private_workflow_records(1, &wallet_id, 0x41);
    let source_key = format!(
        "1|{wallet_id}|{}",
        alloy::hex::encode(pending.output_commitment)
    );
    let source_payload = rmp_serde::to_vec_named(&pending).expect("encode shipped pending row");
    let conflicting_row_id = cache_keys.private_row_id(
        WalletPrivateRecordKind::PendingOutputPoiContext,
        legacy_namespace.wallet_id.as_str(),
        pending.output_commitment.as_slice(),
    );
    drop(store);
    drop(db);
    put_raw_wallet_private_row(
        &db_path,
        PENDING_OUTPUT_CONTEXT_V1,
        &source_key,
        &source_payload,
    );

    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("reopen db for rollback fixture"),
    );
    db.put_opaque_wallet_private_row(
        &legacy_namespace,
        WalletPrivateRecordKind::PendingOutputPoiContext,
        &OpaqueWalletPrivateRow {
            row_id: conflicting_row_id.to_vec(),
            payload: b"preexisting-conflict".to_vec(),
        },
    )
    .expect("seed migration destination conflict");
    assert!(
        DesktopEncryptedWalletCacheStore::new(Arc::clone(&db), &view_session, chain_metadata,)
            .is_err()
    );
    assert_eq!(
        db.list_wallet_private_v1_rows(&legacy_namespace)
            .expect("list rolled-back shipped rows")
            .pending_output_contexts
            .len(),
        1
    );
    assert_eq!(
        db.get_opaque_wallet_private_row(
            &legacy_namespace,
            WalletPrivateRecordKind::PendingOutputPoiContext,
            &conflicting_row_id,
        )
        .expect("load unchanged conflict row")
        .expect("conflict row present")
        .payload,
        b"preexisting-conflict"
    );

    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn encrypted_cache_store_hides_wallet_history_details() {
    use alloy::primitives::{FixedBytes, U256};
    use railgun_wallet::{Note, Utxo, UtxoCommitmentKind, UtxoSource};
    use sync_service::WalletCacheStore;

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let wallet_id = "encrypted-cache-wallet";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, wallet_id)
            .expect("load view session"),
    );
    let chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let wallet_chain_uuid = chain_metadata.wallet_chain_uuid.clone();
    let wallet_cache_key = wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("wallet cache key");
    let cache_store =
        DesktopEncryptedWalletCacheStore::new(Arc::clone(&db), &view_session, chain_metadata)
            .expect("encrypted cache store");
    let wallet_utxo = WalletUtxo {
        utxo: Utxo::new(
            Note {
                token_hash: U256::from_be_bytes([0x44; KEY_LEN]),
                value: U256::from_be_bytes([0x55; KEY_LEN]),
                random: [0x66; 16],
                npk: U256::from_be_bytes([0x77; KEY_LEN]),
            },
            7,
            42,
            UtxoSource {
                tx_hash: FixedBytes::from([0x88; KEY_LEN]),
                block_number: 123,
                block_timestamp: 1_700_000_123,
            },
            UtxoCommitmentKind::Transact,
        ),
        spent: Some(UtxoSource {
            tx_hash: FixedBytes::from([0x99; KEY_LEN]),
            block_number: 124,
            block_timestamp: 1_700_000_124,
        }),
    };

    cache_store
        .replace_wallet_cache_atomically_for_test(
            &wallet_cache_key,
            std::slice::from_ref(&wallet_utxo),
            150,
            Some([0xaa; KEY_LEN]),
        )
        .expect("store encrypted cache");
    let rows = db
        .list_wallet_utxos(&wallet_cache_key)
        .expect("list encrypted cache rows");
    let loaded = cache_store
        .load_wallet_utxos(&wallet_cache_key)
        .expect("load encrypted cache");
    let loaded_meta = cache_store
        .get_wallet_meta(&wallet_cache_key)
        .expect("load updated chain metadata")
        .expect("chain metadata present");

    assert_eq!(rows.len(), 1);
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].utxo.note.token_hash,
        wallet_utxo.utxo.note.token_hash
    );
    assert_eq!(
        loaded[0].utxo.source.tx_hash,
        wallet_utxo.utxo.source.tx_hash
    );
    assert_eq!(loaded[0].spent, wallet_utxo.spent);
    assert_eq!(loaded_meta.last_scanned_block, 150);
    assert_eq!(loaded_meta.last_scanned_block_hash, Some([0xaa; KEY_LEN]));

    let row_key = rows[0].utxo_id.as_bytes();
    let row_payload = &rows[0].payload;
    assert!(!contains_subsequence(row_key, b"1111111111111111"));
    assert!(!contains_subsequence(row_payload, &[0x44; KEY_LEN]));
    assert!(!contains_subsequence(row_payload, &[0x55; KEY_LEN]));
    assert!(!contains_subsequence(row_payload, &[0x66; 16]));
    assert!(!contains_subsequence(row_payload, &[0x77; KEY_LEN]));
    assert!(!contains_subsequence(row_payload, &[0x88; KEY_LEN]));
    assert!(!contains_subsequence(row_payload, &[0x99; KEY_LEN]));

    cache_store
        .replace_wallet_cache_atomically_for_test(&wallet_cache_key, &[], 152, None)
        .expect("commit authoritative empty atomic cache");
    assert!(
        cache_store
            .load_wallet_utxos(&wallet_cache_key)
            .expect("load authoritative empty cache")
            .is_empty()
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn encrypted_cache_upsert_does_not_delete_existing_rows() {
    use alloy::primitives::{FixedBytes, U256};
    use railgun_wallet::{Note, Utxo, UtxoCommitmentKind, UtxoSource};
    use sync_service::WalletCacheStore;

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "encrypted-cache-upsert-wallet";
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, wallet_id)
            .expect("load view session"),
    );
    let mut chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let wallet_chain_uuid = chain_metadata.wallet_chain_uuid.clone();
    let wallet_cache_key = wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("wallet cache key");
    let cache_store = DesktopEncryptedWalletCacheStore::new(
        Arc::clone(&db),
        &view_session,
        chain_metadata.clone(),
    )
    .expect("encrypted cache store");
    let first = WalletUtxo {
        utxo: Utxo::new(
            Note {
                token_hash: U256::from_be_bytes([0x11; KEY_LEN]),
                value: uint!(1_U256),
                random: [0x22; 16],
                npk: U256::from_be_bytes([0x33; KEY_LEN]),
            },
            3,
            1,
            UtxoSource {
                tx_hash: FixedBytes::from([0x44; KEY_LEN]),
                block_number: 101,
                block_timestamp: 1_700_000_101,
            },
            UtxoCommitmentKind::Transact,
        ),
        spent: None,
    };
    let mut second = first.clone();
    second.utxo.position = 2;
    second.utxo.source.tx_hash = FixedBytes::from([0x55; KEY_LEN]);
    let mut rewound_source = first.clone();
    rewound_source.utxo.position = 3;
    rewound_source.utxo.source = UtxoSource {
        tx_hash: FixedBytes::from([0x66; KEY_LEN]),
        block_number: 170,
        block_timestamp: 1_700_000_170,
    };
    let mut rewound_spend = first.clone();
    rewound_spend.utxo.position = 4;
    rewound_spend.utxo.source.tx_hash = FixedBytes::from([0x77; KEY_LEN]);
    rewound_spend.spent = Some(UtxoSource {
        tx_hash: FixedBytes::from([0x88; KEY_LEN]),
        block_number: 170,
        block_timestamp: 1_700_000_170,
    });

    cache_store
        .replace_wallet_cache_atomically_for_test(
            &wallet_cache_key,
            &[first.clone(), second, rewound_source, rewound_spend],
            110,
            None,
        )
        .expect("store full cache");
    let loaded = cache_store
        .load_wallet_utxos(&wallet_cache_key)
        .expect("load full cache");
    assert_eq!(loaded.len(), 4);
    assert!(loaded.iter().any(|utxo| utxo.utxo.position == 1));
    assert!(loaded.iter().any(|utxo| utxo.utxo.position == 2));
    assert!(loaded.iter().any(|utxo| utxo.utxo.position == 3));
    assert!(loaded.iter().any(|utxo| utxo.utxo.position == 4));

    store
        .rewind_wallet_chain_cache_with_session(view_session.as_ref(), &mut chain_metadata, 150)
        .expect("rewind encrypted cache");
    let loaded = cache_store
        .load_wallet_utxos(&wallet_cache_key)
        .expect("load rewound cache");
    assert_eq!(loaded.len(), 3);
    assert!(loaded.iter().any(|utxo| utxo.utxo.position == 1));
    assert!(loaded.iter().any(|utxo| utxo.utxo.position == 2));
    assert!(!loaded.iter().any(|utxo| utxo.utxo.position == 3));
    assert!(
        loaded
            .iter()
            .any(|utxo| utxo.utxo.position == 4 && utxo.spent.is_none())
    );
    let metadata = store
        .load_wallet_chain_metadata(TEST_PASSWORD, &wallet_chain_uuid)
        .expect("load rewound metadata");
    assert_eq!(metadata.last_scanned_block, 149);
    assert_eq!(metadata.last_scanned_block_hash, None);

    cache_store
        .replace_wallet_cache_atomically_for_test(
            &wallet_cache_key,
            std::slice::from_ref(&first),
            160,
            None,
        )
        .expect("replace encrypted cache");
    let loaded = cache_store
        .load_wallet_utxos(&wallet_cache_key)
        .expect("load replaced cache");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].utxo.position, 1);
    let metadata = db
        .get_wallet_meta(&wallet_cache_key)
        .expect("load replaced atomic metadata")
        .expect("atomic metadata present");
    assert_eq!(metadata.last_scanned_block, 160);
    assert_eq!(metadata.last_scanned_block_hash, None);

    store
        .reset_wallet_chain_cache_with_session(view_session.as_ref(), &mut chain_metadata, 160)
        .expect("reset encrypted cache");
    assert!(
        cache_store
            .load_wallet_utxos(&wallet_cache_key)
            .expect("load reset cache")
            .is_empty()
    );
    let metadata = store
        .load_wallet_chain_metadata(TEST_PASSWORD, &wallet_chain_uuid)
        .expect("load reset metadata");
    assert_eq!(metadata.last_scanned_block, 159);
    assert_eq!(metadata.last_scanned_block_hash, None);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn encrypted_cache_repair_retires_incompatible_pending_reset() {
    use alloy::primitives::{FixedBytes, U256};
    use local_db::{WalletPendingResetRecord, WalletSyncActorStateRecord};
    use railgun_wallet::{Note, Utxo, UtxoCommitmentKind, UtxoSource};
    use sync_service::WalletCacheStore;

    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_id = "encrypted-cache-repair-wallet";
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, wallet_id)
            .expect("load view session"),
    );
    let mut chain_metadata = store
        .wallet_chain_metadata_for_session(
            view_session.as_ref(),
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("chain metadata");
    let wallet_chain_uuid = chain_metadata.wallet_chain_uuid.clone();
    let wallet_cache_key = wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("wallet cache key");
    let cache_store = DesktopEncryptedWalletCacheStore::new(
        Arc::clone(&db),
        &view_session,
        chain_metadata.clone(),
    )
    .expect("encrypted cache store");
    let retained = WalletUtxo {
        utxo: Utxo::new(
            Note {
                token_hash: U256::from_be_bytes([0x11; KEY_LEN]),
                value: uint!(1_U256),
                random: [0x22; 16],
                npk: U256::from_be_bytes([0x33; KEY_LEN]),
            },
            3,
            1,
            UtxoSource {
                tx_hash: FixedBytes::from([0x44; KEY_LEN]),
                block_number: 80,
                block_timestamp: 1_700_000_080,
            },
            UtxoCommitmentKind::Transact,
        ),
        spent: None,
    };
    let mut dropped = retained.clone();
    dropped.utxo.position = 2;
    dropped.utxo.source = UtxoSource {
        tx_hash: FixedBytes::from([0x55; KEY_LEN]),
        block_number: 180,
        block_timestamp: 1_700_000_180,
    };
    cache_store
        .replace_wallet_cache_atomically_for_test(
            &wallet_cache_key,
            &[retained, dropped],
            200,
            Some([0xaa; KEY_LEN]),
        )
        .expect("seed encrypted cache");
    db.put_wallet_sync_actor_state(&WalletSyncActorStateRecord {
        chain_id: chain_metadata.chain_id,
        wallet_id: wallet_cache_key.to_string(),
        highest_accepted_reset_intent: 41,
        pending_reset: Some(WalletPendingResetRecord {
            intent_id: 41,
            from_block: 190,
            replay_start_block: 190,
            replay_target_block: 220,
            follow_safe_head: false,
        }),
        updated_at: 1,
    })
    .expect("seed pending reset");

    store
        .rewind_wallet_chain_cache_with_session(view_session.as_ref(), &mut chain_metadata, 90)
        .expect("repair encrypted cache");

    let loaded = cache_store
        .load_wallet_utxos(&wallet_cache_key)
        .expect("load repaired cache");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].utxo.position, 1);
    let wallet_meta = db
        .get_wallet_meta(&wallet_cache_key)
        .expect("load repaired cursor")
        .expect("repaired cursor present");
    assert_eq!(wallet_meta.last_scanned_block, 89);
    assert_eq!(wallet_meta.last_scanned_block_hash, None);
    let stored_chain_metadata = store
        .load_wallet_chain_metadata(TEST_PASSWORD, &wallet_chain_uuid)
        .expect("load repaired encrypted metadata");
    assert_eq!(stored_chain_metadata.start_block, 90);
    assert_eq!(stored_chain_metadata.last_scanned_block, 89);
    assert_eq!(stored_chain_metadata.last_scanned_block_hash, None);
    assert_eq!(
        chain_metadata.wallet_chain_uuid,
        stored_chain_metadata.wallet_chain_uuid
    );
    assert_eq!(
        chain_metadata.wallet_uuid,
        stored_chain_metadata.wallet_uuid
    );
    assert_eq!(chain_metadata.chain_type, stored_chain_metadata.chain_type);
    assert_eq!(chain_metadata.chain_id, stored_chain_metadata.chain_id);
    assert_eq!(chain_metadata.contract, stored_chain_metadata.contract);
    assert_eq!(
        chain_metadata.start_block,
        stored_chain_metadata.start_block
    );
    assert_eq!(
        chain_metadata.last_scanned_block,
        stored_chain_metadata.last_scanned_block
    );
    assert_eq!(
        chain_metadata.last_scanned_block_hash,
        stored_chain_metadata.last_scanned_block_hash
    );
    assert_eq!(
        chain_metadata.poi_read_source,
        stored_chain_metadata.poi_read_source
    );
    let actor_state = db
        .get_wallet_sync_actor_state(chain_metadata.chain_id, wallet_cache_key.as_str())
        .expect("load repaired actor state")
        .expect("repaired actor state present");
    assert_eq!(actor_state.chain_id, chain_metadata.chain_id);
    assert_eq!(actor_state.wallet_id, wallet_cache_key.as_str());
    assert_eq!(actor_state.highest_accepted_reset_intent, 41);
    assert!(actor_state.pending_reset.is_none());
    assert!(actor_state.updated_at > 1);

    drop(cache_store);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
