use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use alloy::primitives::B256;
use local_db::{LOCAL_DB_TABLES, LocalDbTable};
use redb::{Database, ReadableDatabase, ReadableTable};

use super::super::executors::executor_operation_prefix;
use super::super::*;
use super::helpers::*;
use crate::tests::activity::{activity_attempt, activity_entry, received_note};
use crate::{
    ActivityStatus, ActivityUtxoClassification, StatusInputs, SwapRow, UtxoOutput,
    has_unread_received, is_in_flight, owned_hashes, received_rows,
};

const DAY: u64 = 86_400;
const NOW: u64 = 1_800_000_000;
const OLD: u64 = NOW - 40 * DAY;
const RECENT: u64 = NOW - DAY;

fn entry(owner: &ActivityOwner, kind: ActivityKind, created_at: u64, hash: u8) -> ActivityEntry {
    ActivityEntry {
        owner: owner.clone(),
        created_at,
        ..activity_entry(
            kind,
            ActivityMatch::Hashes,
            vec![activity_attempt(hash, ActivityStep::Main, None)],
        )
    }
}

fn selection(age: ActivityClearAge, kinds: &[ActivityKind], received: bool) -> ActivityClear {
    ActivityClear {
        age,
        kinds: kinds.to_vec(),
        received,
    }
}

fn swap(started_at: u64, in_flight: bool) -> SwapRow {
    SwapRow {
        id: SwapUseId::random().unwrap(),
        network: ActivityNetwork::Evm(1),
        started_at,
        legs: Vec::new(),
        hashes: Vec::new(),
        status: if in_flight {
            ActivityStatus::Submitted
        } else {
            ActivityStatus::Confirmed(None)
        },
        in_flight,
        recipient: None,
    }
}

fn ids<'a>(entries: impl IntoIterator<Item = &'a ActivityEntry>) -> BTreeSet<ActivityEntryId> {
    entries.into_iter().map(|entry| entry.id).collect()
}

/// The owner's entries Activity lists, without cleared stubs.
fn shown(journal: &ActivityJournal, owner: &ActivityOwner) -> BTreeSet<ActivityEntryId> {
    ids(journal
        .list(owner)
        .unwrap()
        .iter()
        .filter(|entry| !entry.cleared))
}

/// Hashes of the received rows the journal's wallet derives from `utxos`.
fn received(journal: &ActivityJournal, utxos: &[UtxoOutput]) -> Vec<B256> {
    let entries: Vec<ActivityEntry> = journal
        .scope_owners()
        .unwrap()
        .iter()
        .flat_map(|owner| journal.list(owner).unwrap())
        .collect();
    let hidden_before = journal.markers().unwrap().received_hidden_before;
    received_rows(utxos, &owned_hashes(&entries, &[]), hidden_before)
        .into_iter()
        .map(|row| row.hash)
        .collect()
}

/// Sentinel notes, POI records and an executor record, written with the store closed.
fn put_recovery_rows(db_path: &Path) {
    let db = Database::open(db_path).unwrap();
    let txn = db.begin_write().unwrap();
    let executor = format!("{}sentinel", executor_operation_prefix(TEST_WALLET_ID, 1));
    for (table, key) in [
        (LocalDbTable::WalletUtxo, "note"),
        (LocalDbTable::PendingOutputPoiContextV2, "poi-context"),
        (LocalDbTable::OutputPoiRecoveryV2, "poi-recovery"),
        (LocalDbTable::DesktopWalletVault, executor.as_str()),
    ] {
        txn.open_table(table.definition())
            .unwrap()
            .insert(key, b"sentinel".as_slice())
            .unwrap();
    }
    txn.commit().unwrap();
}

/// Every local-db row except journal records, read with the store closed.
fn non_journal_rows(db_path: &Path) -> BTreeMap<(&'static str, String), Vec<u8>> {
    let db = Database::open(db_path).unwrap();
    let txn = db.begin_read().unwrap();
    let mut rows = BTreeMap::new();
    for info in LOCAL_DB_TABLES {
        let Ok(table) = txn.open_table(info.table.definition()) else {
            continue;
        };
        for row in table.iter().unwrap() {
            let (key, value) = row.unwrap();
            let key = key.value().to_owned();
            if info.table == LocalDbTable::DesktopWalletVault && key.starts_with("activity") {
                continue;
            }
            rows.insert((info.name, key), value.value().to_vec());
        }
    }
    rows
}

#[test]
fn clearing_removes_matching_history_only_and_keeps_its_notes_from_reappearing() {
    use ActivityClearAge::{All, OlderThanDays};
    use ActivityKind::{ContractCall, PrivateSend, Shield, Swap, Unshield};
    use ActivityUtxoClassification::{PrivateOutput, Shield as ShieldNote};

    let (root, db, vault) = desktop_store_with_vault();
    let db_path = db.db_path();
    let view_a = Arc::new(import_wallet_with_metadata(&vault, TEST_WALLET_ID, "A"));
    let view_b = Arc::new(import_wallet_with_metadata(&vault, "wallet-2", "B"));
    let scoped = vault
        .list_active_public_accounts_for_session(&view_a)
        .unwrap()
        .remove(0);
    let global = vault
        .import_public_account(TEST_PASSWORD, &view_a, IMPORT_PRIVATE_KEY_ONE, None, true)
        .unwrap();
    let wallet = ActivityOwner::Wallet(TEST_WALLET_ID.to_owned());
    let scoped = ActivityOwner::Account(scoped.public_account_uuid);
    let global = ActivityOwner::Account(global.public_account_uuid);
    // Never observed, so it reads as submitted, but nothing is running for it.
    let stale = entry(&wallet, PrivateSend, OLD, 0x01);
    let running = entry(&wallet, PrivateSend, OLD, 0x02);
    let unshield = entry(&wallet, Unshield, OLD, 0x03);
    let recent = entry(&wallet, PrivateSend, RECENT, 0x04);
    let shield = entry(&global, Shield, OLD, 0x05);
    let mut call = entry(&global, ContractCall, OLD, 0x06);
    call.source = ActivitySource::WalletConnect {
        dapp: "Railgun".to_owned(),
    };
    let scoped_shield = entry(&scoped, Shield, OLD, 0x07);
    let journal = ActivityJournal::new(db.clone(), view_a.clone()).unwrap();
    for entry in [
        &stale,
        &running,
        &unshield,
        &recent,
        &shield,
        &call,
        &scoped_shield,
    ] {
        journal.append(entry).unwrap();
    }
    drop((journal, view_a, view_b, vault, db));
    put_recovery_rows(&db_path);
    let before = non_journal_rows(&db_path);
    assert_eq!(
        before.get(&("wallet_utxo", "note".to_owned())),
        Some(&b"sentinel".to_vec())
    );

    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .unwrap(),
    );
    let vault = DesktopVaultStore::from_db(db.clone());
    let journal_a = ActivityJournal::new(
        db.clone(),
        Arc::new(
            vault
                .load_view_session(TEST_PASSWORD, TEST_WALLET_ID)
                .unwrap(),
        ),
    )
    .unwrap();
    let journal_b = ActivityJournal::new(
        db.clone(),
        Arc::new(vault.load_view_session(TEST_PASSWORD, "wallet-2").unwrap()),
    )
    .unwrap();
    // Wallet A's notes: from the account's shield, its dapp call that shielded, the scoped
    // account's shield, then a third-party transfer and a third-party shield.
    let utxos = [
        received_note(0, 0x05, RECENT, ShieldNote),
        received_note(1, 0x06, RECENT, ShieldNote),
        received_note(2, 0x07, RECENT, ShieldNote),
        received_note(3, 0x08, OLD, PrivateOutput),
        received_note(4, 0x09, RECENT, ShieldNote),
    ];
    let third_party = [B256::repeat_byte(0x09), B256::repeat_byte(0x08)];
    assert_eq!(received(&journal_a, &utxos), third_party);
    let nothing_running = |_: &ActivityEntry| false;

    // Wallet B clears the global account's shield; A's scoped account is outside B's scope.
    assert_eq!(
        journal_b
            .clear(
                &selection(OlderThanDays(30), &[Shield], false),
                NOW,
                &[],
                nothing_running
            )
            .unwrap(),
        ActivityClearResult {
            cleared: 1,
            kept_in_flight: 0,
        }
    );
    assert_eq!(shown(&journal_a, &global), BTreeSet::from([call.id]));
    assert_eq!(
        shown(&journal_a, &scoped),
        BTreeSet::from([scoped_shield.id])
    );
    assert_eq!(received(&journal_a, &utxos), third_party);

    let running_ids = BTreeSet::from([running.id]);
    let in_flight = |entry: &ActivityEntry| {
        is_in_flight(
            entry,
            &StatusInputs {
                utxos: &[],
                running: &running_ids,
                owned: &HashSet::new(),
            },
        )
    };
    // Swap rows from executor records: an old resolved use, an old unresolved one and a recent one.
    let swaps = [swap(OLD, false), swap(OLD, true), swap(RECENT, false)];
    assert_eq!(
        journal_a
            .clear(
                &selection(OlderThanDays(30), &[PrivateSend, ContractCall, Swap], true),
                NOW,
                &swaps,
                in_flight
            )
            .unwrap(),
        ActivityClearResult {
            cleared: 3,
            kept_in_flight: 2,
        }
    );
    // The stale wallet entry is gone rather than stubbed; the running one, another kind and a
    // newer one stay.
    assert_eq!(
        ids(&journal_a.list(&wallet).unwrap()),
        BTreeSet::from([running.id, unshield.id, recent.id])
    );
    assert_eq!(shown(&journal_a, &global), BTreeSet::new());
    assert_eq!(
        journal_a.markers().unwrap().received_hidden_before,
        NOW - 30 * DAY
    );
    assert_eq!(received(&journal_a, &utxos), [B256::repeat_byte(0x09)]);

    // A longer age never moves the marker back, and stubs aren't cleared again.
    assert_eq!(
        journal_a
            .clear(
                &selection(OlderThanDays(60), &[Shield], true),
                NOW,
                &[],
                nothing_running
            )
            .unwrap()
            .cleared,
        0
    );
    assert_eq!(
        journal_a.markers().unwrap().received_hidden_before,
        NOW - 30 * DAY
    );
    // An already hidden swap isn't counted again, and the unresolved one is still kept.
    assert_eq!(
        journal_a
            .clear(
                &selection(All, &[Shield, Swap], false),
                NOW,
                &swaps,
                nothing_running
            )
            .unwrap(),
        ActivityClearResult {
            cleared: 2,
            kept_in_flight: 1,
        }
    );
    assert_eq!(shown(&journal_a, &scoped), BTreeSet::new());
    assert_eq!(received(&journal_a, &utxos), [B256::repeat_byte(0x09)]);

    drop((journal_a, journal_b, vault, db));
    assert_eq!(non_journal_rows(&db_path), before);
    // The cleared swaps stay hidden once the store is opened again.
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
    assert_eq!(
        ActivityJournal::new(db.clone(), view.clone())
            .unwrap()
            .markers()
            .unwrap()
            .hidden_swap_uses,
        BTreeSet::from([swaps[0].id, swaps[2].id])
    );
    drop((view, vault, db));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn received_rows_turn_unread_after_the_first_catch_up_until_activity_opens() {
    use ActivityUtxoClassification::PrivateOutput;

    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let network = ActivityNetwork::Evm(1);
    let rows = |utxos: &[UtxoOutput]| received_rows(utxos, &HashSet::new(), 0);
    let unread = |utxos: &[UtxoOutput]| {
        has_unread_received(
            &rows(utxos),
            journal.markers().unwrap().last_seen.get(&network).copied(),
        )
    };
    let mut utxos = vec![received_note(0, 0x01, 100, PrivateOutput)];
    assert!(!unread(&utxos));

    journal.init_last_seen(network, 150).unwrap();
    assert!(!unread(&utxos));
    utxos.push(received_note(1, 0x02, 200, PrivateOutput));
    // A later catch-up keeps the marker set by the first.
    journal.init_last_seen(network, 250).unwrap();
    assert!(unread(&utxos));

    journal
        .advance_last_seen(network, rows(&utxos)[0].block_number)
        .unwrap();
    assert!(!unread(&utxos));
    journal.advance_last_seen(network, 100).unwrap();
    assert_eq!(journal.markers().unwrap().last_seen[&network], 200);

    drop((journal, view, vault, db));
    std::fs::remove_dir_all(root).unwrap();
}
