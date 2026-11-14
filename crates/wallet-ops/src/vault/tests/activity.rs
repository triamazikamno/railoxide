use super::super::activity::{
    activity_account_prefix, activity_markers_key, activity_wallet_prefix,
};
use super::super::*;
use super::helpers::*;
use crate::DesktopWalletVaultDbExt as _;
use crate::desktop::{
    ActivitySubmission, PrivateActivity, PublicActivity, observe_activity_inclusion,
    observe_activity_rollback,
};
use crate::tests::activity::{activity_attempt, activity_entry, received_note};
use crate::{
    ActivityConfirmation, ActivityStatus, ActivityUtxoClassification, PublicBroadcasterResultKind,
    StatusInputs, UtxoOutput, entry_status, running_activity_entries,
    subscribe_activity_journal_failures,
};
use alloy::primitives::{Address, B256, U256};
use std::collections::HashSet;

const COUNTERPARTY: Address = Address::repeat_byte(0xc4);
const SENT_INPUT: ActivityInput = ActivityInput {
    tree: 0,
    position: 7,
};

fn entry(
    owner: &ActivityOwner,
    network: ActivityNetwork,
    created_at: u64,
    hash: u8,
) -> ActivityEntry {
    ActivityEntry {
        owner: owner.clone(),
        network,
        created_at,
        legs: vec![ActivityLeg {
            asset: ActivityAsset::Token(Address::repeat_byte(0xa1)),
            amount: U256::from(5),
            direction: ActivityDirection::Out,
        }],
        counterparty: Some(ActivityCounterparty::Evm(COUNTERPARTY)),
        ..activity_entry(
            ActivityKind::PublicSend,
            ActivityMatch::Hashes,
            vec![activity_attempt(hash, ActivityStep::Main, None)],
        )
    }
}

fn entry_key(prefix: &str, entry: &ActivityEntry) -> String {
    format!("{prefix}{}", entry.id.opaque_id())
}

fn activity_keys(db: &DbStore) -> BTreeSet<String> {
    db.list_desktop_wallet_vault_records("activity")
        .unwrap()
        .into_iter()
        .map(|record| record.key)
        .collect()
}

#[test]
fn activity_entries_are_sealed_and_keyed_by_owner_and_random_id_only() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let account = vault
        .list_active_public_accounts_for_session(&view)
        .unwrap()
        .remove(0);
    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let wallet = ActivityOwner::Wallet(TEST_WALLET_ID.to_owned());
    let account_owner = ActivityOwner::Account(account.public_account_uuid.clone());
    let older = entry(&wallet, ActivityNetwork::Evm(137), 1_700_000_100, 0xe1);
    let newer = entry(&wallet, ActivityNetwork::Evm(137), 1_700_000_200, 0xe2);
    let tron = entry(&wallet, ActivityNetwork::Tron, 1_700_000_300, 0xe3);
    let account_entry = entry(
        &account_owner,
        ActivityNetwork::Evm(137),
        1_700_000_400,
        0xe4,
    );
    for entry in [&older, &newer, &tron, &account_entry] {
        journal.append(entry).unwrap();
    }

    let wallet_prefix = activity_wallet_prefix(TEST_WALLET_ID);
    let account_prefix = activity_account_prefix(&account.public_account_uuid);
    assert_eq!(
        activity_keys(&db),
        BTreeSet::from([
            entry_key(&wallet_prefix, &older),
            entry_key(&wallet_prefix, &newer),
            entry_key(&wallet_prefix, &tron),
            entry_key(&account_prefix, &account_entry),
        ])
    );
    assert_eq!(
        journal
            .list_for_network(&wallet, ActivityNetwork::Evm(137))
            .unwrap(),
        [newer.clone(), older.clone()]
    );
    assert_eq!(
        journal.list(&wallet).unwrap(),
        [tron.clone(), newer.clone(), older.clone()]
    );
    assert_eq!(journal.list(&account_owner).unwrap(), [account_entry]);
    assert!(matches!(
        journal.list(&ActivityOwner::Wallet("other-wallet".to_owned())),
        Err(ActivityStoreError::OwnerMismatch)
    ));

    let outcome = ActivityAttemptOutcome::Included {
        block: 9,
        timestamp: 1_700_000_500,
        success: true,
    };
    let updated = journal
        .update(&wallet, older.id, |entry| {
            entry.attempts[0].outcome = Some(outcome);
        })
        .unwrap();
    assert_eq!(updated.attempts[0].outcome, Some(outcome));
    let stub = journal.clear_to_stub(&wallet, newer.id).unwrap();
    assert!(stub.cleared && stub.counterparty.is_none() && stub.legs.is_empty());
    assert_eq!(stub.attempts[0].hash, newer.attempts[0].hash);
    journal
        .update_markers(|markers| {
            markers.received_hidden_before = 1_700_000_000;
            markers.last_seen.insert(ActivityNetwork::Tron, 77);
            markers.last_seen.insert(ActivityNetwork::Evm(137), 78);
        })
        .unwrap();
    journal.delete(&wallet, tron.id).unwrap();

    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    assert_eq!(journal.list(&wallet).unwrap(), [updated, stub]);
    let markers = journal.markers().unwrap();
    assert_eq!(markers.received_hidden_before, 1_700_000_000);
    assert_eq!(
        markers.last_seen,
        BTreeMap::from([(ActivityNetwork::Tron, 77), (ActivityNetwork::Evm(137), 78)])
    );
    for record in db.list_desktop_wallet_vault_records("activity").unwrap() {
        assert!(!contains_subsequence(
            &record.payload,
            COUNTERPARTY.as_slice()
        ));
        assert!(!contains_subsequence(
            &record.payload,
            B256::repeat_byte(0xe1).as_slice()
        ));
    }

    drop(journal);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hardware_wallet_activity_needs_its_hardware_view() {
    let (root, db, vault) = desktop_store_with_vault();
    let descriptor = test_hardware_descriptor(0);
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
            0,
            &test_hardware_wallet(0),
            &metadata,
            &test_hardware_view_access_key(0),
        )
        .unwrap();
    let view = Arc::new(load_test_hardware_view_session(
        &vault,
        TEST_WALLET_ID,
        &descriptor,
    ));
    let account = vault
        .import_public_account(TEST_PASSWORD, &view, IMPORT_PRIVATE_KEY_ONE, None, true)
        .unwrap();
    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let wallet = ActivityOwner::Wallet(TEST_WALLET_ID.to_owned());
    let wallet_entry = entry(&wallet, ActivityNetwork::Evm(1), 1, 0xe1);
    let account_entry = entry(
        &ActivityOwner::Account(account.public_account_uuid.clone()),
        ActivityNetwork::Evm(1),
        2,
        0xe2,
    );
    journal.append(&wallet_entry).unwrap();
    journal.append(&account_entry).unwrap();
    journal
        .update_markers(|markers| markers.received_hidden_before = 5)
        .unwrap();

    assert!(matches!(
        vault.load_view_session(TEST_PASSWORD, TEST_WALLET_ID),
        Err(VaultError::HardwareWalletViewRequiresDevice)
    ));
    let password_view = vault.unlock_view(TEST_PASSWORD).unwrap();
    let password_opens = |kind: RecordKind, key: &str| {
        let payload = db.get_desktop_wallet_vault_record(key).unwrap().unwrap();
        let record: EncryptedRecord = rmp_serde::from_slice(&payload).unwrap();
        password_view.decrypt_record(kind, key, &record).is_ok()
    };
    assert!(!password_opens(
        RecordKind::ActivityEntry,
        &entry_key(&activity_wallet_prefix(TEST_WALLET_ID), &wallet_entry)
    ));
    assert!(!password_opens(
        RecordKind::ActivityMarkers,
        &activity_markers_key(TEST_WALLET_ID)
    ));
    // Account entries are readable wherever the account is in scope.
    assert!(password_opens(
        RecordKind::ActivityEntry,
        &entry_key(
            &activity_account_prefix(&account.public_account_uuid),
            &account_entry
        )
    ));
    assert_eq!(journal.list(&wallet).unwrap(), [wallet_entry]);
    assert_eq!(journal.markers().unwrap().received_hidden_before, 5);

    drop(journal);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn owner_deletion_removes_activity_and_deactivation_keeps_it() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let other_view = Arc::new(import_wallet_with_metadata(&vault, "wallet-10", "Other"));
    let derived_public = vault
        .list_active_public_accounts_for_session(&view)
        .unwrap()
        .remove(0);
    let derived_tron = vault
        .add_derived_tron_account(TEST_PASSWORD, &view, None, None)
        .unwrap();
    let imported_public = vault
        .import_public_account(TEST_PASSWORD, &view, IMPORT_PRIVATE_KEY_ONE, None, false)
        .unwrap();
    let imported_tron = vault
        .import_tron_account(TEST_PASSWORD, &view, IMPORT_PRIVATE_KEY_TWO, None, false)
        .unwrap();
    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let wallet = ActivityOwner::Wallet(TEST_WALLET_ID.to_owned());
    let derived_public_owner = ActivityOwner::Account(derived_public.public_account_uuid);
    let derived_tron_owner = ActivityOwner::Account(derived_tron.tron_account_uuid.clone());
    let imported_public_owner = ActivityOwner::Account(imported_public.public_account_uuid.clone());
    let imported_tron_owner = ActivityOwner::Account(imported_tron.tron_account_uuid.clone());
    for (owner, network) in [
        (&wallet, ActivityNetwork::Evm(1)),
        (&derived_public_owner, ActivityNetwork::Evm(1)),
        (&derived_tron_owner, ActivityNetwork::Tron),
        (&imported_public_owner, ActivityNetwork::Evm(1)),
        (&imported_tron_owner, ActivityNetwork::Tron),
    ] {
        journal.append(&entry(owner, network, 1, 0xe1)).unwrap();
    }
    journal.update_markers(|_| {}).unwrap();
    let other_entry = entry(
        &ActivityOwner::Wallet("wallet-10".to_owned()),
        ActivityNetwork::Evm(1),
        1,
        0xe2,
    );
    ActivityJournal::new(db.clone(), other_view.clone())
        .unwrap()
        .append(&other_entry)
        .unwrap();

    vault
        .set_derived_tron_account_active(&view, &derived_tron.tron_account_uuid, false)
        .unwrap();
    assert_eq!(journal.list(&derived_tron_owner).unwrap().len(), 1);

    vault
        .delete_imported_public_account(&view, &imported_public.public_account_uuid)
        .unwrap();
    assert_eq!(
        journal.list(&imported_public_owner).unwrap(),
        [] as [ActivityEntry; 0]
    );
    vault
        .delete_imported_tron_account(&view, &imported_tron.tron_account_uuid)
        .unwrap();
    assert_eq!(
        journal.list(&imported_tron_owner).unwrap(),
        [] as [ActivityEntry; 0]
    );

    vault
        .delete_wallet_for_session(&view, TEST_WALLET_ID)
        .unwrap();
    assert_eq!(
        activity_keys(&db),
        BTreeSet::from([entry_key(
            &activity_wallet_prefix("wallet-10"),
            &other_entry
        )])
    );
    assert!(matches!(
        journal.append(&entry(&wallet, ActivityNetwork::Evm(1), 2, 0xe3)),
        Err(ActivityStoreError::Unavailable)
    ));

    drop(journal);
    drop(other_view);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

/// 5 of token `0x11…` spending the note at `SENT_INPUT`, with a fee of 1 of token `0x22…`.
pub(super) fn private_activity(
    kind: ActivityKind,
    counterparty: ActivityCounterparty,
) -> PrivateActivity {
    PrivateActivity {
        chain_id: 1,
        kind,
        token: Address::repeat_byte(0x11),
        amount: U256::from(5),
        counterparty,
        fee: Some(ActivityFee {
            asset: ActivityAsset::Token(Address::repeat_byte(0x22)),
            amount: U256::ONE,
        }),
        gas_payer: Some(Address::repeat_byte(0x33)),
        inputs: vec![SENT_INPUT],
    }
}

fn attempt_hashes(entry: &ActivityEntry) -> Vec<B256> {
    entry.attempts.iter().map(|attempt| attempt.hash).collect()
}

#[test]
fn signed_private_attempts_share_one_entry_written_at_their_handoff() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    // The vault refuses to delete its last active wallet.
    let _other_view = import_wallet_with_metadata(&vault, "wallet-10", "Other");
    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let wallet = ActivityOwner::Wallet(TEST_WALLET_ID.to_owned());
    let recipient = ActivityCounterparty::Private("0zk1recipient".to_owned());
    let submission = ActivitySubmission::private(
        &vault,
        &view,
        private_activity(ActivityKind::PrivateSend, recipient.clone()),
        false,
    );
    // Nothing is written before the first signed attempt, so a pre-send error leaves no entry.
    assert_eq!(journal.list(&wallet).unwrap(), [] as [ActivityEntry; 0]);

    // The pre-transport callback records each attempt before its send, whatever the send's
    // response: a first attempt, a gas retry, and a sponsored relay resubmitting the retry.
    for hash in [1, 2, 2] {
        submission.add_attempt(B256::repeat_byte(hash));
    }
    let [entry]: [ActivityEntry; 1] = journal.list(&wallet).unwrap().try_into().unwrap();
    assert_eq!(
        (entry.network, entry.kind, entry.match_key.clone()),
        (
            ActivityNetwork::Evm(1),
            ActivityKind::PrivateSend,
            ActivityMatch::Inputs(vec![SENT_INPUT])
        )
    );
    assert_eq!(
        entry.legs,
        [ActivityLeg {
            asset: ActivityAsset::Token(Address::repeat_byte(0x11)),
            amount: U256::from(5),
            direction: ActivityDirection::Out,
        }]
    );
    assert_eq!(entry.counterparty, Some(recipient.clone()));
    assert_eq!(
        (entry.fee.map(|fee| fee.amount), entry.gas_payer),
        (Some(U256::ONE), Some(Address::repeat_byte(0x33)))
    );
    assert_eq!(
        attempt_hashes(&entry),
        [B256::repeat_byte(1), B256::repeat_byte(2)]
    );
    assert!(running_activity_entries().contains(&entry.id));
    drop(submission);
    assert!(!running_activity_entries().contains(&entry.id));

    // A journal that can't be written doesn't fail the submission but raises the notice, and a
    // deleted wallet's entry isn't recreated.
    let orphaned = ActivitySubmission::private(
        &vault,
        &view,
        private_activity(ActivityKind::PrivateSend, recipient),
        false,
    );
    vault
        .delete_wallet_for_session(&view, TEST_WALLET_ID)
        .unwrap();
    let failures = subscribe_activity_journal_failures();
    orphaned.add_attempt(B256::repeat_byte(3));
    assert_eq!(activity_keys(&db), BTreeSet::new());
    assert!(failures.has_changed().unwrap());

    drop(orphaned);
    drop(journal);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn relayed_private_entries_stay_unless_the_broadcaster_definitely_failed() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let wallet = ActivityOwner::Wallet(TEST_WALLET_ID.to_owned());
    let submission = || {
        let submission = ActivitySubmission::private(
            &vault,
            &view,
            private_activity(
                ActivityKind::Unshield,
                ActivityCounterparty::Evm(COUNTERPARTY),
            ),
            false,
        );
        submission.begin();
        submission
    };

    // The entry is written before publishing. A definite failure, or an error before
    // publishing, removes it.
    let failed = PublicBroadcasterResultKind::Failed {
        error: String::new(),
    };
    for result in [Some(&failed), None] {
        let submission = submission();
        assert_eq!(journal.list(&wallet).unwrap().len(), 1);
        submission.finish_relay(result);
        assert_eq!(journal.list(&wallet).unwrap(), [] as [ActivityEntry; 0]);
    }

    // A timeout keeps it with its inputs and no hash, and spent inputs confirm it later.
    let timed_out = submission();
    timed_out.finish_relay(Some(&PublicBroadcasterResultKind::TimedOut));
    let [entry]: [ActivityEntry; 1] = journal.list(&wallet).unwrap().try_into().unwrap();
    assert!(entry.relay_timed_out);
    assert_eq!(attempt_hashes(&entry), [] as [B256; 0]);
    let mut note = received_note(7, 0x01, 100, ActivityUtxoClassification::PrivateOutput);
    let status = |note: &UtxoOutput| {
        entry_status(
            &entry,
            &StatusInputs {
                utxos: std::slice::from_ref(note),
                running: &running_activity_entries(),
                owned: &HashSet::new(),
            },
        )
    };
    assert_eq!(status(&note), ActivityStatus::Unconfirmed);
    note.is_spent = true;
    note.spent_tx_hash = Some(alloy::hex::encode_prefixed(B256::repeat_byte(0xaa)));
    note.spent_block_number = Some(20);
    assert!(matches!(status(&note), ActivityStatus::Confirmed(Some(_))));
    drop(timed_out);

    // A `Submitted` response adds the broadcaster's hash.
    let submitted = submission();
    submitted.finish_relay(Some(&PublicBroadcasterResultKind::Submitted {
        tx_hash: alloy::hex::encode_prefixed(B256::repeat_byte(4)),
    }));
    let relayed = journal
        .list(&wallet)
        .unwrap()
        .into_iter()
        .find(|relayed| relayed.id != entry.id)
        .unwrap();
    assert_eq!(attempt_hashes(&relayed), [B256::repeat_byte(4)]);

    drop(submitted);
    drop(journal);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn observed_outcomes_persist_under_the_submitting_owner() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let account = vault
        .list_active_public_accounts_for_session(&view)
        .unwrap()
        .remove(0);
    let public = |kind: ActivityKind, counterparty: Option<ActivityCounterparty>| {
        ActivitySubmission::public(
            &vault,
            &view,
            PublicActivity {
                account_uuid: account.public_account_uuid.clone(),
                chain_id: 1,
                kind,
                legs: Vec::new(),
                counterparty,
            },
        )
    };
    // Observation is process-wide by hash, so these bytes are used by no other test.
    let hash = B256::repeat_byte;

    // A shield stopped after its included approval.
    let shield = public(ActivityKind::Shield, None);
    shield.add_step_attempt(hash(0xd2), ActivityStep::Approve);
    observe_activity_inclusion(hash(0xd2), 10, 100, true);
    // A public send and a private send whose attempts are handed off after the user selected
    // another wallet. The send's gas retry is included and the private send reverts.
    let send = public(
        ActivityKind::PublicSend,
        Some(ActivityCounterparty::Evm(COUNTERPARTY)),
    );
    let private = ActivitySubmission::private(
        &vault,
        &view,
        private_activity(
            ActivityKind::PrivateSend,
            ActivityCounterparty::Private("0zk1recipient".to_owned()),
        ),
        false,
    );
    let selected = Arc::new(import_wallet_with_metadata(&vault, "wallet-10", "Other"));
    send.add_attempt(hash(0xd3));
    send.add_attempt(hash(0xd4));
    observe_activity_inclusion(hash(0xd4), 12, 120, true);
    private.add_attempt(hash(0xd5));
    observe_activity_inclusion(hash(0xd5), 13, 130, false);
    // An inclusion the running observer then rolls back past.
    let orphaned = public(ActivityKind::ContractCall, None);
    orphaned.add_attempt(hash(0xd6));
    observe_activity_inclusion(hash(0xd6), 14, 140, true);
    observe_activity_rollback([&hash(0xd6)], 13);
    drop((shield, send, private, orphaned));

    // Read back from a reopened journal with no network at all.
    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let running = running_activity_entries();
    let status = |entry: &ActivityEntry| {
        entry_status(
            entry,
            &StatusInputs {
                utxos: &[],
                running: &running,
                owned: &HashSet::new(),
            },
        )
    };
    let account_entries = journal
        .list(&ActivityOwner::Account(account.public_account_uuid))
        .unwrap();
    assert_eq!(account_entries.len(), 3);
    let of_kind = |kind: ActivityKind| {
        account_entries
            .iter()
            .find(|entry| entry.kind == kind)
            .unwrap()
    };
    assert_eq!(
        status(of_kind(ActivityKind::Shield)),
        ActivityStatus::Incomplete
    );
    let send = of_kind(ActivityKind::PublicSend);
    assert_eq!(attempt_hashes(send), [hash(0xd3), hash(0xd4)]);
    assert_eq!(
        status(send),
        ActivityStatus::Confirmed(Some(ActivityConfirmation {
            hash: hash(0xd4),
            block: 12,
            timestamp: Some(120),
        }))
    );
    assert_eq!(
        status(of_kind(ActivityKind::ContractCall)),
        ActivityStatus::Submitted
    );
    let [private]: [ActivityEntry; 1] = journal
        .list(&ActivityOwner::Wallet(TEST_WALLET_ID.to_owned()))
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(status(&private), ActivityStatus::Failed { fee_spent: true });
    assert_eq!(
        ActivityJournal::new(db.clone(), selected.clone())
            .unwrap()
            .list(&ActivityOwner::Wallet("wallet-10".to_owned()))
            .unwrap(),
        [] as [ActivityEntry; 0]
    );

    drop(journal);
    drop(selected);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn tron_sends_keep_their_decided_outcome_under_the_tron_account() {
    use crate::tron::confirmation::TronSendOutcome;
    use crate::tron::send::tron_attempt_outcome;

    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let tron = vault
        .add_derived_tron_account(TEST_PASSWORD, &view, None, None)
        .unwrap();
    let owner = ActivityOwner::Account(tron.tron_account_uuid.clone());
    let receiver = Address::repeat_byte(0x41);
    // Each send is journaled at its handoff and given the outcome solidified blocks decided.
    // Observation is process-wide by hash, so these bytes are used by no other test.
    let send = |txid: u8, outcome| {
        let activity = ActivitySubmission::tron(
            &vault,
            &view,
            tron.tron_account_uuid.clone(),
            ActivityLeg {
                asset: ActivityAsset::Native,
                amount: U256::from(5),
                direction: ActivityDirection::Out,
            },
            receiver,
        );
        activity.add_attempt(B256::repeat_byte(txid));
        activity.record_outcome(
            B256::repeat_byte(txid),
            tron_attempt_outcome(outcome, 1_700_000_000_999),
        );
    };
    send(
        0xe7,
        TronSendOutcome::Complete {
            amount: U256::from(5),
            block: 70,
        },
    );
    send(0xe8, TronSendOutcome::Dropped);

    // Read back from a reopened journal with no network at all.
    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let running = running_activity_entries();
    let entries = journal
        .list_for_network(&owner, ActivityNetwork::Tron)
        .unwrap();
    let status = |txid| {
        let entry = entries
            .iter()
            .find(|entry| attempt_hashes(entry) == [B256::repeat_byte(txid)])
            .unwrap();
        assert_eq!(
            (entry.kind, entry.counterparty.clone()),
            (
                ActivityKind::PublicSend,
                Some(ActivityCounterparty::Tron(receiver))
            )
        );
        entry_status(
            entry,
            &StatusInputs {
                utxos: &[],
                running: &running,
                owned: &HashSet::new(),
            },
        )
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(
        status(0xe7),
        ActivityStatus::Confirmed(Some(ActivityConfirmation {
            hash: B256::repeat_byte(0xe7),
            block: 70,
            timestamp: Some(1_700_000_000),
        }))
    );
    assert_eq!(status(0xe8), ActivityStatus::Failed { fee_spent: false });

    drop(journal);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dapp_transactions_are_tagged_with_their_source() {
    use crate::gateway::GatewayDraftExecution;
    use crate::walletconnect::{WalletConnectDecodedCallKind, WalletConnectDecodedTransaction};
    use alloy::rpc::types::TransactionRequest;

    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let account = vault
        .list_active_public_accounts_for_session(&view)
        .unwrap()
        .remove(0);
    let token = Address::repeat_byte(0xa1);

    // A WalletConnect ERC-20 transfer is a public send of that token to its recipient.
    crate::public_wallet::walletconnect_activity(
        &vault,
        &view,
        &account.public_account_uuid,
        1,
        "Dapp",
        &TransactionRequest::default(),
        Some(&WalletConnectDecodedTransaction {
            target: Some(token),
            native_value: U256::ZERO,
            kind: WalletConnectDecodedCallKind::Erc20Transfer {
                recipient: COUNTERPARTY,
                amount: U256::from(5),
            },
        }),
    )
    .add_attempt(B256::repeat_byte(0xe9));
    // A gateway private draft's submission carries its execution's origin.
    let execution =
        GatewayDraftExecution::private("execution".to_owned()).with_origin("Browser".to_owned());
    ActivitySubmission::private(
        &vault,
        &view,
        private_activity(
            ActivityKind::PrivateSend,
            ActivityCounterparty::Private("0zk1recipient".to_owned()),
        ),
        false,
    )
    .with_gateway_origin(execution.origin())
    .add_attempt(B256::repeat_byte(0xea));

    let journal = ActivityJournal::new(db.clone(), view.clone()).unwrap();
    let [send]: [ActivityEntry; 1] = journal
        .list(&ActivityOwner::Account(account.public_account_uuid))
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        (send.kind, send.source, send.counterparty, send.legs),
        (
            ActivityKind::PublicSend,
            ActivitySource::WalletConnect {
                dapp: "Dapp".to_owned()
            },
            Some(ActivityCounterparty::Evm(COUNTERPARTY)),
            vec![ActivityLeg {
                asset: ActivityAsset::Token(token),
                amount: U256::from(5),
                direction: ActivityDirection::Out,
            }],
        )
    );
    let [private]: [ActivityEntry; 1] = journal
        .list(&ActivityOwner::Wallet(TEST_WALLET_ID.to_owned()))
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        (private.kind, private.source),
        (
            ActivityKind::PrivateSend,
            ActivitySource::Gateway {
                origin: "Browser".to_owned()
            }
        )
    );

    drop(journal);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
