#[allow(clippy::wildcard_imports)]
use super::*;

/// The `activity` mode's in-memory Activity on `chain_id` and on Tron: an entry of each kind and
/// status, three received rows, one row selected and a received row newer than the network's
/// last-seen block. It takes the USDC and WETH of `chain_id`, or Ethereum's where it lists none.
/// `None` in every other mode, or without either token.
pub(in crate::root) fn activity(chain_id: u64) -> Option<ActivityStandIn> {
    const HOUR: u64 = 3_600;
    const DAY: u64 = 24 * HOUR;
    if !matches!(mode(), Some(Mode::Activity)) {
        return None;
    }
    let token = |symbol: &str| {
        [chain_id, 1]
            .into_iter()
            .find_map(|chain| known_tokens_for_chain(chain).find(|token| token.symbol == symbol))
    };
    let (usdc, weth) = (token("USDC")?, token("WETH")?);
    let tron_usdt =
        known_tokens_for_chain(railgun_ui::TRON_CHAIN_ID).find(|token| token.symbol == "USDT")?;
    let now = now_unix();
    let ago = |seconds: u64| now.saturating_sub(seconds);
    let hash = |seed: &str| keccak256(format!("ui-fixture-activity-{seed}"));
    let evm = ActivityNetwork::Evm(chain_id);
    let leg = |asset: ActivityAsset, raw: u128, direction: ActivityDirection| ActivityLeg {
        asset,
        amount: U256::from(raw),
        direction,
    };
    let usdc_out = |raw| {
        leg(
            ActivityAsset::Token(usdc.token),
            raw,
            ActivityDirection::Out,
        )
    };
    let weth_out = |raw| {
        leg(
            ActivityAsset::Token(weth.token),
            raw,
            ActivityDirection::Out,
        )
    };
    let attempt =
        |seed: &str, step: ActivityStep, outcome: Option<ActivityAttemptOutcome>| ActivityAttempt {
            hash: hash(seed),
            step,
            outcome,
        };
    let included = |block: u64, seconds: u64, success: bool| {
        Some(ActivityAttemptOutcome::Included {
            block,
            timestamp: ago(seconds),
            success,
        })
    };
    let entry =
        |network: ActivityNetwork, kind: ActivityKind, created_at: u64| -> Option<ActivityEntry> {
            Some(ActivityEntry {
                id: ActivityEntryId::random().ok()?,
                owner: ActivityOwner::Wallet("ui-fixture".to_owned()),
                network,
                kind,
                source: ActivitySource::Wallet,
                created_at,
                legs: Vec::new(),
                counterparty: None,
                fee: None,
                gas_payer: None,
                match_key: ActivityMatch::Hashes,
                attempts: Vec::new(),
                relay_timed_out: false,
                cleared: false,
            })
        };
    let recipient = ActivityCounterparty::Evm(Address::repeat_byte(0x4e));
    let shield_hash = hash("shield");
    let private_send_hash = hash("private-send");

    // Pinned: a public send whose submission is still running.
    let running = ActivityEntry {
        legs: vec![usdc_out(25_000_000)],
        counterparty: Some(recipient.clone()),
        attempts: vec![attempt("running", ActivityStep::Main, None)],
        ..entry(evm, ActivityKind::PublicSend, ago(40))?
    };
    // A swap whose first order expired and whose retry is open, so it is in progress. The
    // private swaps view doesn't hold its record.
    let records = vec![staged_swap(None, Some(SpeedUp::Open))?];
    let mut swaps = swap_rows(chain_id, &records);
    // Shown as bridging to the public send's recipient on another chain, whatever the record
    // delivers.
    for swap in &mut swaps {
        swap.recipient = Some(SwapRowRecipient::Evm {
            address: Address::repeat_byte(0x4e),
            chain_id: if chain_id == 8453 { 42161 } else { 8453 },
        });
    }
    // Confirmed by its input note, spent in the snapshot.
    let private_send = ActivityEntry {
        legs: vec![weth_out(100_000_000_000_000_000)],
        counterparty: Some(ActivityCounterparty::Private(
            "0zk1qyk9nn28x0u3rwn5pknglda68wrn7gw6anjw8gg94mcj6eq5u48tlrv7j6fe3z53lama02nutwtcqc979wnce0qwly4y7w4rls5cq040g7z8eagshxrw5ajy990".to_owned(),
        )),
        fee: Some(ActivityFee {
            asset: ActivityAsset::Token(weth.token),
            amount: U256::from(1_200_000_000_000_000_u128),
        }),
        match_key: ActivityMatch::Inputs(vec![ActivityInput {
            tree: 1,
            position: 90_001,
        }]),
        attempts: vec![ActivityAttempt {
            hash: private_send_hash,
            step: ActivityStep::Main,
            outcome: None,
        }],
        ..entry(evm, ActivityKind::PrivateSend, ago(2 * HOUR))?
    };
    // The broadcaster timed out, and its input note is still unspent.
    let unshield = ActivityEntry {
        legs: vec![usdc_out(40_000_000)],
        counterparty: Some(ActivityCounterparty::Evm(Address::repeat_byte(0x7a))),
        source: ActivitySource::Gateway {
            origin: "https://app.example.org".to_owned(),
        },
        match_key: ActivityMatch::Inputs(vec![ActivityInput {
            tree: 1,
            position: 90_002,
        }]),
        relay_timed_out: true,
        ..entry(evm, ActivityKind::Unshield, ago(30 * 60))?
    };
    // Its shield notes are in the snapshot, so it reads as into this wallet.
    let shield = ActivityEntry {
        legs: vec![weth_out(250_000_000_000_000_000)],
        attempts: vec![
            attempt(
                "shield-approve",
                ActivityStep::Approve,
                included(UTXO_HEAD - 3_001, 3 * HOUR, true),
            ),
            ActivityAttempt {
                hash: shield_hash,
                step: ActivityStep::Main,
                outcome: included(UTXO_HEAD - 3_000, 3 * HOUR, true),
            },
        ],
        ..entry(evm, ActivityKind::Shield, ago(3 * HOUR))?
    };
    // Only its approval was sent.
    let incomplete_shield = ActivityEntry {
        legs: vec![usdc_out(500_000_000)],
        attempts: vec![attempt(
            "incomplete-approve",
            ActivityStep::Approve,
            included(UTXO_HEAD - 7_000, DAY, true),
        )],
        ..entry(evm, ActivityKind::Shield, ago(DAY))?
    };
    let reverted = ActivityEntry {
        legs: vec![usdc_out(10_000_000)],
        counterparty: Some(recipient),
        fee: Some(ActivityFee {
            asset: ActivityAsset::Native,
            amount: U256::from(400_000_000_000_000_u128),
        }),
        gas_payer: Some(Address::repeat_byte(0x22)),
        attempts: vec![attempt(
            "reverted",
            ActivityStep::Main,
            included(UTXO_HEAD - 1_500, 5 * HOUR, false),
        )],
        ..entry(evm, ActivityKind::PublicSend, ago(5 * HOUR))?
    };
    // Three amounts, so its row shows one and "+2 more".
    let contract_call = ActivityEntry {
        legs: vec![
            leg(
                ActivityAsset::Native,
                100_000_000_000_000_000,
                ActivityDirection::Out,
            ),
            usdc_out(150_000_000),
            leg(
                ActivityAsset::Token(weth.token),
                90_000_000_000_000_000,
                ActivityDirection::In,
            ),
        ],
        counterparty: Some(ActivityCounterparty::Evm(Address::repeat_byte(0x66))),
        source: ActivitySource::WalletConnect {
            dapp: "Uniswap".to_owned(),
        },
        attempts: vec![attempt(
            "contract-call",
            ActivityStep::Main,
            included(UTXO_HEAD - 14_000, 2 * DAY, true),
        )],
        ..entry(evm, ActivityKind::ContractCall, ago(2 * DAY))?
    };
    // Never observed: it stays submitted.
    let governance = ActivityEntry {
        counterparty: Some(ActivityCounterparty::Evm(Address::repeat_byte(0x33))),
        attempts: vec![attempt("governance", ActivityStep::Main, None)],
        ..entry(evm, ActivityKind::Governance, ago(3 * DAY))?
    };
    let refund = ActivityEntry {
        legs: vec![usdc_out(5_000_000_000)],
        counterparty: Some(ActivityCounterparty::Evm(Address::repeat_byte(0x4e))),
        attempts: vec![attempt(
            "refund",
            ActivityStep::Main,
            Some(ActivityAttemptOutcome::Dropped),
        )],
        ..entry(evm, ActivityKind::Refund, ago(4 * DAY))?
    };
    let tron_send = ActivityEntry {
        legs: vec![leg(
            ActivityAsset::Token(tron_usdt.token),
            25_000_000,
            ActivityDirection::Out,
        )],
        counterparty: Some(ActivityCounterparty::Tron(Address::repeat_byte(0x5c))),
        attempts: vec![attempt(
            "tron-send",
            ActivityStep::Main,
            included(78_000_000, 6 * HOUR, true),
        )],
        ..entry(
            ActivityNetwork::Tron,
            ActivityKind::PublicSend,
            ago(6 * HOUR),
        )?
    };
    let tron_dropped = ActivityEntry {
        legs: vec![leg(
            ActivityAsset::Native,
            100_000_000,
            ActivityDirection::Out,
        )],
        counterparty: Some(ActivityCounterparty::Tron(Address::repeat_byte(0x5d))),
        attempts: vec![attempt(
            "tron-dropped",
            ActivityStep::Main,
            Some(ActivityAttemptOutcome::Dropped),
        )],
        ..entry(ActivityNetwork::Tron, ActivityKind::PublicSend, ago(DAY))?
    };
    let running_id = running.id;
    let selected = ActivityRowId::Entry(reverted.id);

    let received = hash("received");
    let utxos = vec![
        // The private send's spent input and the unshield's unspent one, both from the shield.
        UtxoOutput {
            is_spent: true,
            spent_tx_hash: Some(alloy::hex::encode_prefixed(private_send_hash)),
            spent_block_number: Some(UTXO_HEAD - 600),
            spent_block_timestamp: Some(ago(2 * HOUR)),
            ..activity_note(
                weth,
                150_000_000_000_000_000,
                90_001,
                "Shield",
                shield_hash,
                UTXO_HEAD - 3_000,
                ago(3 * HOUR),
            )
        },
        activity_note(
            weth,
            100_000_000_000_000_000,
            90_002,
            "Shield",
            shield_hash,
            UTXO_HEAD - 3_000,
            ago(3 * HOUR),
        ),
        // Newer than the last-seen block, so the tab shows it unread.
        activity_note(
            usdc,
            120_000_000,
            90_010,
            "Private Output",
            received,
            UTXO_HEAD - 20,
            ago(15 * 60),
        ),
        activity_note(
            weth,
            10_000_000_000_000_000,
            90_011,
            "Private Output",
            received,
            UTXO_HEAD - 20,
            ago(15 * 60),
        ),
        activity_note(
            weth,
            500_000_000_000_000_000,
            90_003,
            "Shield",
            hash("shield-received"),
            UTXO_HEAD - 5_000,
            ago(20 * HOUR),
        ),
        activity_note(
            usdc,
            5_000_000_000,
            90_004,
            "Blocked Shield",
            hash("blocked-shield"),
            UTXO_HEAD - 9_000,
            ago(2 * DAY),
        ),
    ];
    Some(ActivityStandIn {
        data: ActivityData {
            entries: vec![
                running,
                private_send,
                unshield,
                shield,
                incomplete_shield,
                reverted,
                contract_call,
                governance,
                refund,
                tron_send,
                tron_dropped,
            ],
            markers: ActivityMarkers {
                last_seen: [
                    (evm, UTXO_HEAD - 1_000),
                    (ActivityNetwork::Tron, 78_000_000),
                ]
                .into_iter()
                .collect(),
                ..ActivityMarkers::default()
            },
            records,
            swaps,
        },
        utxos,
        running: std::collections::BTreeSet::from([running_id]),
        selected: Some(selected),
        chain_id,
    })
}

/// A note of the `activity` mode: `raw` units of `token` at `position` in tree 1, created by
/// `source` in `block` at `timestamp`, with the classification the UTXO table shows.
fn activity_note(
    token: KnownTokenInfo,
    raw: u128,
    position: u64,
    classification: &str,
    source: B256,
    block: u64,
    timestamp: u64,
) -> UtxoOutput {
    let blocked = classification == "Blocked Shield";
    UtxoOutput {
        tree: 1,
        position,
        token: token.token.to_checksum(None),
        value: raw.to_string(),
        commitment_kind: if classification == "Private Output" {
            "Transact"
        } else {
            "Shield"
        }
        .to_owned(),
        activity_classification: classification.to_owned(),
        blocked_shield_rescue: None,
        commitment: alloy::hex::encode_prefixed(keccak256(format!("ui-fixture-note-{position}"))),
        npk: alloy::hex::encode_prefixed(keccak256(format!("ui-fixture-npk-{position}"))),
        blinded_commitment: alloy::hex::encode_prefixed(keccak256(format!(
            "ui-fixture-blinded-{position}"
        ))),
        poi_statuses: std::collections::BTreeMap::new(),
        ppoi_state: if blocked {
            UtxoPpoiState::ShieldBlocked
        } else {
            UtxoPpoiState::Valid
        },
        ppoi_last_submission_at: None,
        poi_spendable: !blocked,
        source_tx_hash: alloy::hex::encode_prefixed(source),
        source_block_number: block,
        source_block_timestamp: timestamp,
        is_spent: false,
        pending_new: false,
        pending_spent: false,
        local_pending_spent: false,
        spent_tx_hash: None,
        spent_block_number: None,
        spent_block_timestamp: None,
    }
}
