use std::sync::Arc;

use alloy::primitives::{B256, address};
use railgun_wallet::{Note, UtxoSource};
use sync_service::indexed_artifacts::{ChainScope, ChainType};
use sync_service::{
    AmountStatisticsCoveredBlock, AmountStatisticsSnapshot, RepeatedShieldAmount,
    RepeatedUnshieldAmount, ShieldValueBucketCount, TreeShieldAmountCount, TreeShieldValueBuckets,
};

use super::PrivacyFactorKind as Kind;
use super::PrivacyLevel::{Bad, Caution, Good};
use super::*;
use crate::tests::activity::{activity_attempt, activity_entry};
use crate::vault::{
    ActivityAttempt, ActivityMatch, PublicAccountScope, PublicAccountSource, PublicAccountStatus,
};

const USDC: Address = address!("0x1111111111111111111111111111111111111111");
const WETH: Address = address!("0x2222222222222222222222222222222222222222");
const RECIPIENT: Address = address!("0x3333333333333333333333333333333333333333");
const NOW: u64 = 1_800_000_000;
const DAY: u64 = 24 * 60 * 60;

fn stats(token: Address) -> TokenAmountStatistics {
    TokenAmountStatistics {
        token,
        pool_balance: U256::ZERO,
        shields_last_24h: 0,
        shields_last_30d: 0,
        unshields_last_24h: 0,
        unshields_last_30d: 0,
        repeated_shield_amounts: Vec::new(),
        shield_value_buckets: Vec::new(),
        repeated_unshield_amounts: Vec::new(),
    }
}

fn available(tokens: Vec<TokenAmountStatistics>) -> AmountStatisticsStatus {
    AmountStatisticsStatus::Available(Arc::new(AmountStatisticsSnapshot {
        encoding_version: 1,
        scope: ChainScope {
            chain_type: ChainType::Evm,
            chain_id: 1,
            railgun_contract: Address::ZERO,
        },
        covered_block: AmountStatisticsCoveredBlock {
            number: 1,
            hash: B256::ZERO,
            timestamp: NOW,
        },
        tokens,
    }))
}

fn deposits(value: u64, trees: &[(u32, u64)]) -> RepeatedShieldAmount {
    RepeatedShieldAmount {
        value: U256::from(value),
        trees: trees
            .iter()
            .map(|&(tree, all_time)| TreeShieldAmountCount {
                tree,
                all_time,
                last_30d: 0,
            })
            .collect(),
    }
}

fn exits(value: u64, last_30d: u64) -> RepeatedUnshieldAmount {
    RepeatedUnshieldAmount {
        value: U256::from(value),
        last_30d,
    }
}

fn buckets(tree: u32, counts: &[(u16, u64)]) -> TreeShieldValueBuckets {
    TreeShieldValueBuckets {
        tree,
        buckets: counts
            .iter()
            .map(|&(bit_length, count)| ShieldValueBucketCount { bit_length, count })
            .collect(),
    }
}

fn shield_input(statistics: &AmountStatisticsStatus) -> ShieldHintInput<'_> {
    ShieldHintInput {
        statistics,
        token: USDC,
        amount: U256::from(1_000_000_000_u64),
        shield_fee_bps: U256::from(25),
        balance: U256::from(1_500_000_000_u64),
        current_tree: Some(2),
        route: NetworkRoute::Private,
    }
}

fn shield_lines_of(hints: &ShieldHints) -> &ShieldLines {
    hints.lines.as_ref().expect("statistics are available")
}

fn unshield_input<'a>(
    statistics: &'a AmountStatisticsStatus,
    planned: Option<&'a [ResolvedPlannedUnshield]>,
) -> UnshieldHintInput<'a> {
    UnshieldHintInput {
        statistics,
        token: USDC,
        planned,
        own_deposits: &[],
        sender: UnshieldSender::CalldataOnly,
        recipient_deposited: false,
        route: NetworkRoute::Private,
        now: NOW,
        entered_amount_with_deducted_fee: None,
        max_amount: None,
    }
}

fn unshield_lines_of(hints: &UnshieldHints) -> &UnshieldLines {
    hints
        .lines
        .as_ref()
        .expect("statistics are available")
        .as_ref()
        .expect("a plan exists")
}

fn input(value: u64, is_shield: bool, age_secs: u64) -> ResolvedPlannedInput {
    ResolvedPlannedInput {
        value: U256::from(value),
        is_shield,
        source_timestamp: NOW - age_secs,
    }
}

/// An unshield of `value` spending one older note from a private transfer.
fn exit(tree: u32, value: u64) -> ResolvedPlannedUnshield {
    ResolvedPlannedUnshield {
        tree,
        value: U256::from(value),
        change: U256::ZERO,
        inputs: vec![input(value, false, 2 * DAY)],
    }
}

fn note(tree: u32, position: u64, value: u64, kind: UtxoCommitmentKind) -> Utxo {
    Utxo::new(
        Note::new_unshield(Address::ZERO, USDC, U256::from(value)),
        tree,
        position,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 1,
            block_timestamp: NOW - 2 * DAY,
        },
        kind,
    )
}

fn plan(tree: u32, positions: &[u64], value: u64, change: u64) -> PlannedUnshield {
    PlannedUnshield {
        tree,
        inputs: positions
            .iter()
            .map(|&position| SelectedInputIdentity { tree, position })
            .collect(),
        value: U256::from(value),
        change: U256::from(change),
    }
}

fn find(factors: &[PrivacyFactor], is_kind: impl Fn(&Kind) -> bool) -> Option<PrivacyFactor> {
    factors.iter().find(|factor| is_kind(&factor.kind)).copied()
}

#[test]
fn crowd_scale_is_bad_up_to_one_and_good_from_thirty() {
    for (count, level) in [
        (0, Bad),
        (1, Bad),
        (2, Caution),
        (7, Caution),
        (29, Caution),
        (30, Good),
        (437, Good),
    ] {
        assert_eq!(crowd_level(count), level, "{count}");
    }
}

#[test]
fn shield_counts_deposits_with_the_entered_amounts_note_value() {
    let mut usdc = stats(USDC);
    usdc.repeated_shield_amounts = vec![deposits(997_500_000, &[(0, 1_000), (1, 204)])];
    let statistics = available(vec![usdc]);
    // 1,000 USDC at 25 basis points is a 997.5 note; 1,370 has no row.
    for (amount, note_value, expected, level) in [
        (1_000_000_000_u64, 997_500_000_u64, Some(1_204), Good),
        (1_370_000_000, 1_366_575_000, None, Bad),
    ] {
        let hints = evaluate_shield_hints(&ShieldHintInput {
            amount: U256::from(amount),
            ..shield_input(&statistics)
        });
        assert_eq!(hints.note_value, U256::from(note_value), "{amount}");
        assert_eq!(shield_lines_of(&hints).deposits, expected, "{amount}");
        assert!(
            hints.factors.contains(&PrivacyFactor {
                level,
                kind: Kind::AmountCrowd { deposits: expected },
            }),
            "{amount}"
        );
    }
}

#[test]
fn shield_pool_share_is_good_below_ten_percent_and_otherwise_a_caution() {
    // (pool balance or token absent, note value, expected share, level)
    for (balance, note_value, expected, level) in [
        (
            Some(2_350_000_000_000_u64),
            997_500_000_u64,
            Some(424),
            Good,
        ),
        (Some(10_000_000_000), 1_000_000_000, Some(100_000), Caution),
        (Some(0), 997_500_000, None, Caution),
        (None, 997_500_000, None, Caution),
    ] {
        let tokens = balance.map_or_else(Vec::new, |balance| {
            let mut usdc = stats(USDC);
            usdc.pool_balance = U256::from(balance);
            vec![usdc]
        });
        let statistics = available(tokens);
        let hints = evaluate_shield_hints(&ShieldHintInput {
            amount: U256::from(note_value),
            shield_fee_bps: U256::ZERO,
            ..shield_input(&statistics)
        });
        let pool = expected.zip(balance).map(|(share_ppm, balance)| PoolShare {
            balance: U256::from(balance),
            share_ppm,
        });
        assert_eq!(
            find(&hints.factors, |kind| matches!(kind, Kind::PoolShare(_))),
            Some(PrivacyFactor {
                level,
                kind: Kind::PoolShare(pool),
            }),
            "{balance:?}"
        );
    }
}

#[test]
fn shield_lists_common_amounts_around_the_entered_one() {
    let common = |amount: u64, count: u64, entered: bool| CommonAmount {
        amount: U256::from(amount),
        count,
        entered,
        within_balance: amount <= 1_500,
    };
    // Without a fee, note values are amounts; the balance is 1,500. Unsorted: six common amounts
    // below 1,000, seven above it and a nearer one used too rarely.
    let around = vec![
        deposits(1_200, &[(0, 32)]),
        deposits(300, &[(0, 30)]),
        deposits(1_500, &[(0, 96)]),
        deposits(800, &[(0, 38)]),
        deposits(990, &[(0, 29)]),
        deposits(1_100, &[(0, 45)]),
        deposits(600, &[(0, 210)]),
        deposits(1_600, &[(0, 640)]),
        deposits(1_400, &[(0, 31)]),
        deposits(400, &[(0, 33)]),
        deposits(1_300, &[(0, 50)]),
        deposits(700, &[(0, 980)]),
        deposits(500, &[(0, 35)]),
        deposits(1_450, &[(0, 60)]),
    ];
    for (name, rows, amount, expected) in [
        (
            "the nearest five on each side",
            around,
            1_000_u64,
            vec![
                common(400, 33, false),
                common(500, 35, false),
                common(600, 210, false),
                common(700, 980, false),
                common(800, 38, false),
                common(1_000, 0, true),
                common(1_100, 45, false),
                common(1_200, 32, false),
                common(1_300, 50, false),
                common(1_400, 31, false),
                common(1_450, 60, false),
            ],
        ),
        (
            "the entered amount is common",
            vec![deposits(700, &[(0, 980)]), deposits(500, &[(0, 35)])],
            700,
            vec![common(500, 35, false), common(700, 980, true)],
        ),
        (
            "a common amount above the balance",
            vec![deposits(990, &[(0, 29)]), deposits(1_600, &[(0, 640)])],
            1_000,
            vec![common(1_000, 0, true), common(1_600, 640, false)],
        ),
        ("no repeated amount", Vec::new(), 1_000, Vec::new()),
    ] {
        let repeated = !rows.is_empty();
        let lines = usdc_shield_lines(rows, amount, 0);
        assert_eq!(lines.common_amounts, expected, "{name}");
        assert_eq!(lines.repeated_amounts, repeated, "{name}");
    }
}

#[test]
fn shield_lists_common_amounts_as_the_amounts_to_type() {
    // At 25 basis points, typing 800 leaves a note of 798.
    let lines = usdc_shield_lines(vec![deposits(798, &[(0, 38)])], 1_000, 25);
    let listed: Vec<_> = lines
        .common_amounts
        .iter()
        .map(|common| common.amount)
        .collect();
    assert_eq!(listed, [U256::from(800_u64), U256::from(1_000_u64)]);
}

/// The Shield lines for `amount` of USDC from a balance of 1,500, with `rows` as its repeated
/// shield amounts.
fn usdc_shield_lines(rows: Vec<RepeatedShieldAmount>, amount: u64, fee_bps: u64) -> ShieldLines {
    let mut usdc = stats(USDC);
    usdc.repeated_shield_amounts = rows;
    let statistics = available(vec![usdc]);
    let hints = evaluate_shield_hints(&ShieldHintInput {
        amount: U256::from(amount),
        shield_fee_bps: U256::from(fee_bps),
        balance: U256::from(1_500_u64),
        ..shield_input(&statistics)
    });
    shield_lines_of(&hints).clone()
}

#[test]
fn shield_counts_the_token_in_the_current_tree() {
    let mut usdc = stats(USDC);
    usdc.shield_value_buckets = vec![buckets(1, &[(30, 500)]), buckets(2, &[(20, 3), (30, 4)])];
    let statistics = available(vec![usdc]);
    for (current_tree, expected) in [
        (
            Some(2),
            Some((
                TreeDeposits {
                    tree: 2,
                    deposits: 7,
                },
                Caution,
            )),
        ),
        (
            Some(3),
            Some((
                TreeDeposits {
                    tree: 3,
                    deposits: 0,
                },
                Bad,
            )),
        ),
        (None, None),
    ] {
        let hints = evaluate_shield_hints(&ShieldHintInput {
            current_tree,
            ..shield_input(&statistics)
        });
        assert_eq!(
            shield_lines_of(&hints).current_tree,
            expected.map(|(deposits, _)| deposits)
        );
        assert_eq!(
            find(&hints.factors, |kind| matches!(
                kind,
                Kind::CurrentTreeCrowd(_)
            )),
            expected.map(|(deposits, level)| PrivacyFactor {
                level,
                kind: Kind::CurrentTreeCrowd(deposits),
            })
        );
    }
}

#[test]
fn unshield_looks_up_each_planned_value_with_a_deducted_fee_added_back() {
    let mut usdc = stats(USDC);
    usdc.repeated_unshield_amounts = vec![exits(600_000_000, 2), exits(1_000_000_000, 40)];
    let mut weth = stats(WETH);
    weth.repeated_unshield_amounts = vec![exits(1_000_000_000, 999)];
    let statistics = available(vec![usdc, weth]);
    // Each user entered 1,000 USDC. With a 2 USDC broadcaster fee taken from it, the fee is
    // charged to the first planned unshield and added back there for the lookups.
    // (name, planned values, entered amount when the fee is taken from it, expected lines)
    for (name, values, entered, expected) in [
        (
            "a deducted fee, WETH has the same value",
            vec![998_000_000_u64],
            Some(1_000_000_000_u64),
            vec![(1_000_000_000_u64, Some(40))],
        ),
        (
            "no fee taken from the amount",
            vec![998_000_000],
            None,
            vec![(998_000_000, None)],
        ),
        (
            "one note in each of two trees",
            vec![600_000_000, 400_000_000],
            None,
            vec![(600_000_000, Some(2)), (400_000_000, None)],
        ),
        (
            "a deducted fee over two trees",
            vec![598_000_000, 400_000_000],
            Some(1_000_000_000),
            vec![(600_000_000, Some(2)), (400_000_000, None)],
        ),
    ] {
        let planned: Vec<_> = values.iter().map(|&value| exit(1, value)).collect();
        let hints = evaluate_unshield_hints(&UnshieldHintInput {
            entered_amount_with_deducted_fee: entered.map(U256::from),
            ..unshield_input(&statistics, Some(&planned))
        });
        let lines: Vec<_> = unshield_lines_of(&hints)
            .unshields
            .iter()
            .map(|unshield| (unshield.value.to::<u64>(), unshield.exits_30d))
            .collect();
        assert_eq!(lines, expected, "{name}");
    }
}

#[test]
fn unshield_detects_a_direct_exit_and_counts_the_other_deposits_in_its_tree() {
    use UtxoCommitmentKind::{Shield, Transact};

    let mut usdc = stats(USDC);
    usdc.repeated_shield_amounts = vec![deposits(997_500_000, &[(4, 3), (5, 40)])];
    let statistics = available(vec![usdc]);
    // Spends the wallet's 997.5 note in tree 4, where 3 deposits have that value: 2 others.
    // (name, input note kind, unshield value, change, other deposits)
    for (name, kind, value, change, expected) in [
        ("equals the deposit", Shield, 997_500_000, 0, Some(2)),
        ("fee from the same note", Shield, 995_500_000, 0, Some(2)),
        ("change of 1%", Shield, 987_525_000, 9_975_000, Some(2)),
        ("partial exit", Shield, 597_500_000, 400_000_000, None),
        ("private transfer", Transact, 997_500_000, 0, None),
    ] {
        let spent = note(4, 7, 997_500_000, kind);
        let own = own_shield_deposits(&[WalletUtxo::new(spent.clone())], USDC);
        let planned = resolve_planned_unshields(&[plan(4, &[7], value, change)], &[spent])
            .expect("the selected input resolves");
        let hints = evaluate_unshield_hints(&UnshieldHintInput {
            own_deposits: &own,
            ..unshield_input(&statistics, Some(&planned))
        });
        assert_eq!(
            unshield_lines_of(&hints).unshields[0].direct_exit,
            expected.map(|deposits| TreeDeposits { tree: 4, deposits }),
            "{name}"
        );
    }
}

#[test]
fn two_planned_unshields_are_evaluated_each_with_one_direct_exit_factor() {
    let mut usdc = stats(USDC);
    usdc.repeated_shield_amounts = vec![deposits(600, &[(2, 40)])];
    let statistics = available(vec![usdc]);
    let notes = [
        note(1, 0, 600, UtxoCommitmentKind::Shield),
        note(2, 0, 600, UtxoCommitmentKind::Shield),
    ];
    let planned =
        resolve_planned_unshields(&[plan(1, &[0], 600, 0), plan(2, &[0], 400, 200)], &notes)
            .expect("the selected inputs resolve");
    let own = own_shield_deposits(&notes.map(WalletUtxo::new), USDC);
    let hints = evaluate_unshield_hints(&UnshieldHintInput {
        own_deposits: &own,
        ..unshield_input(&statistics, Some(&planned))
    });
    let lines = unshield_lines_of(&hints);
    let unshields: Vec<_> = lines
        .unshields
        .iter()
        .map(|unshield| (unshield.value, unshield.direct_exit))
        .collect();
    let first_direct = TreeDeposits {
        tree: 1,
        deposits: 0,
    };
    assert_eq!(
        unshields,
        [
            (U256::from(600), Some(first_direct)),
            (U256::from(400), None)
        ]
    );
    let direct: Vec<_> = hints
        .factors
        .iter()
        .filter(|factor| matches!(factor.kind, Kind::DirectExit(_)))
        .collect();
    assert_eq!(
        direct,
        [&PrivacyFactor {
            level: Bad,
            kind: Kind::DirectExit(first_direct),
        }]
    );
}

#[test]
fn unshield_funding_crowd_is_the_lowest_across_planned_unshields() {
    let mut usdc = stats(USDC);
    usdc.shield_value_buckets = vec![
        buckets(1, &[(31, 400)]),
        buckets(2, &[(30, 100), (31, 7), (40, 5)]),
    ];
    let statistics = available(vec![usdc]);
    // 2^29 falls in bucket 30.
    let value = 1 << 29;
    let expected = TreeDeposits {
        tree: 2,
        deposits: 12,
    };
    for (name, trees) in [("one tree", vec![2]), ("two trees", vec![1, 2])] {
        let planned: Vec<_> = trees.iter().map(|&tree| exit(tree, value)).collect();
        let hints = evaluate_unshield_hints(&unshield_input(&statistics, Some(&planned)));
        assert_eq!(unshield_lines_of(&hints).funding_crowd, expected, "{name}");
        assert!(
            hints.factors.contains(&PrivacyFactor {
                level: Caution,
                kind: Kind::FundingCrowd(expected),
            }),
            "{name}"
        );
    }
}

#[test]
fn recent_exits_are_good_only_when_every_planned_value_is_common() {
    for (name, exits_per_value, expected) in [
        ("common", vec![90], Some(90)),
        ("uncommon", vec![3], None),
        ("one of two common", vec![90, 3], None),
    ] {
        let mut usdc = stats(USDC);
        let values = [1_000_u64, 2_000];
        usdc.repeated_unshield_amounts = values
            .iter()
            .zip(&exits_per_value)
            .map(|(&value, &count)| exits(value, count))
            .collect();
        let statistics = available(vec![usdc]);
        let planned: Vec<_> = values[..exits_per_value.len()]
            .iter()
            .map(|&value| exit(1, value))
            .collect();
        let hints = evaluate_unshield_hints(&unshield_input(&statistics, Some(&planned)));
        assert_eq!(
            find(&hints.factors, |kind| matches!(
                kind,
                Kind::RecentExits { .. }
            )),
            expected.map(|exits_30d| PrivacyFactor {
                level: Good,
                kind: Kind::RecentExits { exits_30d },
            }),
            "{name}"
        );
    }
}

#[test]
fn unshield_lists_common_exit_amounts_only_for_a_single_unshield() {
    let mut usdc = stats(USDC);
    usdc.repeated_unshield_amounts = vec![
        exits(3_000, 31),
        exits(200, 4),
        exits(1_000, 45),
        exits(100, 5),
        exits(2_000, 60),
    ];
    let statistics = available(vec![usdc]);
    let common = |amount: u64, count: u64, entered: bool| CommonAmount {
        amount: U256::from(amount),
        count,
        entered,
        within_balance: amount <= 2_000,
    };
    // 200 has too few exits, and 3,000 is above the form's maximum of 2,000.
    for (name, values, expected) in [
        (
            "one unshield",
            vec![500_u64],
            vec![
                common(100, 5, false),
                common(500, 0, true),
                common(1_000, 45, false),
                common(2_000, 60, false),
                common(3_000, 31, false),
            ],
        ),
        ("two unshields", vec![300, 200], Vec::new()),
    ] {
        let planned: Vec<_> = values.iter().map(|&value| exit(1, value)).collect();
        let hints = evaluate_unshield_hints(&UnshieldHintInput {
            max_amount: Some(U256::from(2_000)),
            ..unshield_input(&statistics, Some(&planned))
        });
        let lines = unshield_lines_of(&hints);
        assert_eq!(lines.common_exit_amounts, expected, "{name}");
    }
}

#[test]
fn unshield_close_to_an_own_deposit_counts_the_other_deposits_near_it() {
    let mut usdc = stats(USDC);
    // Within 5% of the own deposit of 1,000: 951, 1,000 and 1,050, but not 949 or 1,051.
    usdc.repeated_shield_amounts = vec![
        deposits(949, &[(0, 100)]),
        deposits(951, &[(0, 3)]),
        deposits(1_000, &[(1, 10), (2, 5)]),
        deposits(1_050, &[(0, 7)]),
        deposits(1_051, &[(0, 100)]),
    ];
    let statistics = available(vec![usdc]);
    let own = [OwnDeposit {
        tree: 1,
        value: U256::from(1_000),
    }];
    // 3, then 15 less the own deposit, then 7.
    let close = Some(PrivacyFactor {
        level: Caution,
        kind: Kind::CloseToOwnDeposit {
            deposit: U256::from(1_000),
            others: 24,
        },
    });
    let spends_it = ResolvedPlannedUnshield {
        inputs: vec![input(1_000, true, 2 * DAY)],
        ..exit(1, 1_000)
    };
    for (name, unshield, expected) in [
        ("5% above", exit(1, 1_050), close),
        ("5% below", exit(1, 950), close),
        ("just above 5%", exit(1, 1_051), None),
        ("just below 5%", exit(1, 949), None),
        ("a direct exit of it", spends_it, None),
    ] {
        let planned = [unshield];
        let hints = evaluate_unshield_hints(&UnshieldHintInput {
            own_deposits: &own,
            ..unshield_input(&statistics, Some(&planned))
        });
        let factor = find(&hints.factors, |kind| {
            matches!(kind, Kind::CloseToOwnDeposit { .. })
        });
        assert_eq!(factor, expected, "{name}");
    }
}

#[test]
fn quiet_period_cautions_on_a_lull_or_a_quiet_token() {
    for (exits_30d, exits_24h, suggest_waiting) in
        [(600, 6, Some(true)), (600, 18, None), (9, 0, Some(false))]
    {
        let mut usdc = stats(USDC);
        usdc.unshields_last_30d = exits_30d;
        usdc.unshields_last_24h = exits_24h;
        let statistics = available(vec![usdc]);
        let hints = evaluate_unshield_hints(&unshield_input(&statistics, None));
        assert_eq!(
            find(&hints.factors, |kind| matches!(
                kind,
                Kind::QuietPeriod { .. }
            )),
            suggest_waiting.map(|suggest_waiting| PrivacyFactor {
                level: Caution,
                kind: Kind::QuietPeriod {
                    exits_24h,
                    exits_30d,
                    suggest_waiting,
                },
            }),
            "{exits_30d} {exits_24h}"
        );
        // Without a plan, only the token's activity comes from the snapshot.
        assert_eq!(hints.lines, Ok(None));
        assert!(
            hints
                .factors
                .iter()
                .all(|factor| matches!(factor.kind, Kind::QuietPeriod { .. } | Kind::Network(_))),
            "{:?}",
            hints.factors
        );
    }
}

#[test]
fn recent_deposit_cautions_with_its_age() {
    for (age_secs, expected) in [(20 * 60, Some(20 * 60)), (DAY + 60 * 60, None)] {
        let planned = [ResolvedPlannedUnshield {
            inputs: vec![input(1_000, true, age_secs)],
            ..exit(1, 1_000)
        }];
        let hints = evaluate_unshield_hints(&unshield_input(
            &AmountStatisticsStatus::NotConfigured,
            Some(&planned),
        ));
        assert_eq!(
            find(&hints.factors, |kind| matches!(
                kind,
                Kind::RecentDeposit { .. }
            )),
            expected.map(|age_secs| PrivacyFactor {
                level: Caution,
                kind: Kind::RecentDeposit { age_secs },
            }),
            "{age_secs}"
        );
    }
}

#[test]
fn sender_is_bad_for_a_public_account_and_good_for_a_broadcaster() {
    for (sender, expected) in [
        (UnshieldSender::PublicAccount, Some(Bad)),
        (UnshieldSender::Broadcaster, Some(Good)),
        (UnshieldSender::CalldataOnly, None),
    ] {
        let hints = evaluate_unshield_hints(&UnshieldHintInput {
            sender,
            ..unshield_input(&AmountStatisticsStatus::NotConfigured, None)
        });
        assert_eq!(
            find(&hints.factors, |kind| matches!(kind, Kind::Sender(_))),
            expected.map(|level| PrivacyFactor {
                level,
                kind: Kind::Sender(sender),
            }),
            "{sender:?}"
        );
    }
}

#[test]
fn factors_list_bad_then_caution_then_good() {
    let mut usdc = stats(USDC);
    usdc.unshields_last_30d = 600;
    usdc.unshields_last_24h = 18;
    usdc.repeated_unshield_amounts = vec![exits(1_000, 90)];
    let statistics = available(vec![usdc]);
    // Tree 1 holds no deposit large enough, so the funding crowd is bad and comes in last.
    let planned = [exit(1, 1_000)];
    let hints = evaluate_unshield_hints(&UnshieldHintInput {
        sender: UnshieldSender::Broadcaster,
        route: NetworkRoute::Direct,
        ..unshield_input(&statistics, Some(&planned))
    });
    let levels: Vec<_> = hints.factors.iter().map(|factor| factor.level).collect();
    assert_eq!(levels, [Bad, Caution, Good, Good]);
}

#[test]
fn unavailable_statistics_leave_only_the_local_factors() {
    let planned = [ResolvedPlannedUnshield {
        inputs: vec![input(1_000, true, 60)],
        ..exit(1, 1_000)
    }];
    for (status, notice) in [
        (
            AmountStatisticsStatus::NotConfigured,
            StatisticsNotice::NotConfigured,
        ),
        (
            AmountStatisticsStatus::NotYetAvailable,
            StatisticsNotice::NotYetAvailable,
        ),
        (
            AmountStatisticsStatus::OutOfDate,
            StatisticsNotice::OutOfDate,
        ),
    ] {
        let shield = evaluate_shield_hints(&shield_input(&status));
        assert_eq!(shield.lines, Err(notice));
        assert_eq!(
            shield.factors,
            [PrivacyFactor {
                level: Good,
                kind: Kind::Network(NetworkRoute::Private),
            }]
        );
        let unshield = evaluate_unshield_hints(&UnshieldHintInput {
            sender: UnshieldSender::PublicAccount,
            recipient_deposited: true,
            route: NetworkRoute::Direct,
            ..unshield_input(&status, Some(&planned))
        });
        assert_eq!(unshield.lines, Err(notice));
        assert_eq!(
            unshield.factors,
            [
                PrivacyFactor {
                    level: Bad,
                    kind: Kind::Sender(UnshieldSender::PublicAccount),
                },
                PrivacyFactor {
                    level: Bad,
                    kind: Kind::RecipientDeposited,
                },
                PrivacyFactor {
                    level: Caution,
                    kind: Kind::RecentDeposit { age_secs: 60 },
                },
                PrivacyFactor {
                    level: Caution,
                    kind: Kind::Network(NetworkRoute::Direct),
                },
            ],
            "{notice:?}"
        );
    }
}

/// Whether `recipient` deposited on `chain_id`, given one Shield entry with `attempt` owned by a
/// profile account at [`RECIPIENT`].
fn deposited(recipient: Address, chain_id: u64, cleared: bool, attempt: ActivityAttempt) -> bool {
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_owned(),
        address: RECIPIENT,
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::Global,
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let entry = ActivityEntry {
        owner: ActivityOwner::Account(account.public_account_uuid.clone()),
        cleared,
        ..activity_entry(ActivityKind::Shield, ActivityMatch::Hashes, vec![attempt])
    };
    recipient_deposited(recipient, chain_id, &[account], &[entry])
}

#[test]
fn recipient_deposited_needs_an_included_successful_shielding_transaction() {
    let included = |success| {
        Some(ActivityAttemptOutcome::Included {
            block: 5,
            timestamp: 50,
            success,
        })
    };
    let shield = activity_attempt(2, ActivityStep::Main, included(true));
    let approval = activity_attempt(1, ActivityStep::Approve, included(true));
    let failed_approval = activity_attempt(1, ActivityStep::Approve, included(false));
    let reverted = activity_attempt(2, ActivityStep::Main, included(false));
    let pending = activity_attempt(2, ActivityStep::Main, None);
    let dropped = activity_attempt(2, ActivityStep::Main, Some(ActivityAttemptOutcome::Dropped));
    for (name, attempt, expected) in [
        ("included and succeeded", shield, true),
        ("only the approval", approval, false),
        ("failed approval", failed_approval, false),
        ("reverted", reverted, false),
        ("pending", pending, false),
        ("dropped", dropped, false),
    ] {
        assert_eq!(deposited(RECIPIENT, 1, false, attempt), expected, "{name}");
    }
    // An observer can still write an outcome after the entry was cleared to a stub.
    for (name, recipient, chain_id, cleared) in [
        ("cleared stub", RECIPIENT, 1, true),
        ("not a profile account", WETH, 1, false),
        ("Shield on another chain", RECIPIENT, 10, false),
    ] {
        assert!(!deposited(recipient, chain_id, cleared, shield), "{name}");
    }
}
