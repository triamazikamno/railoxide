//! Amount privacy hints for the Shield and Unshield forms, evaluated from the amount-statistics
//! snapshot and local wallet data. Nothing here sends a request, blocks an action or calls an
//! amount safe. Results are figures and levels; the UI owns every word.

use std::collections::BTreeMap;

use alloy::primitives::{Address, U256};
use railgun_wallet::tx::{PlannedUnshield, SelectedInputIdentity};
use railgun_wallet::{Utxo, UtxoCommitmentKind, WalletUtxo};
use sync_service::{AmountStatisticsStatus, TokenAmountStatistics, amount_value_bucket};

use crate::vault::{
    ActivityAttemptOutcome, ActivityEntry, ActivityKind, ActivityNetwork, ActivityOwner,
    ActivityStep, PublicAccountMetadata,
};
use crate::{FEE_BASIS_POINTS_DENOMINATOR, railgun_protocol_fee_amount};
use common_amounts::{common_amounts_around, common_exit_amounts, shield_candidates};
use own_deposits::close_to_own_deposit;

/// A crowd factor is good from this count up.
const CROWD_GOOD_MIN: u64 = 30;
/// A crowd factor is bad up to this count.
const CROWD_BAD_MAX: u64 = 1;
/// The common amounts list only amounts at least this many deposits used.
const COMMON_AMOUNT_MIN_DEPOSITS: u64 = 30;
/// The common amounts list shows at most this many amounts on each side of the entered one.
const COMMON_AMOUNTS_PER_SIDE: usize = 5;
/// A direct exit's change is at most 1/100 of its note's value.
const DIRECT_EXIT_CHANGE_DIVISOR: u64 = 100;
const PARTS_PER_MILLION: u64 = 1_000_000;
/// A Shield note value of at least 10% of the pool balance is a caution, and below it good.
const POOL_SHARE_CAUTION_PPM: u64 = 100_000;
/// A token with at least this many exits in 30 days is busy enough to have lulls.
const QUIET_BUSY_MIN_EXITS_30D: u64 = 120;
/// A busy token is in a lull when its 24-hour exits are below half its 30-day daily average,
/// `exits_24h < exits_30d / 30 / 2`, compared as `exits_24h * 60 < exits_30d`.
const QUIET_LULL_FACTOR: u64 = 30 * 2;
/// A deposit younger than this is recent.
const RECENT_DEPOSIT_SECS: u64 = 24 * 60 * 60;
/// Recent exits are a good factor when every planned value has at least this many.
const RECENT_EXITS_GOOD_MIN: u64 = 30;
/// The common exit amounts list only values with at least this many exits in the last 30 days,
/// lower than the all-time deposit threshold so a month of exits still offers a choice.
const COMMON_EXIT_MIN_EXITS_30D: u64 = 5;
/// An exit within 1/20 (5%) of one of the wallet's own deposits is close to it.
const OWN_DEPOSIT_MATCH_DIVISOR: u64 = 20;

/// Ordered so that sorting puts bad first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrivacyLevel {
    Bad,
    Caution,
    Good,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivacyFactor {
    pub level: PrivacyLevel,
    pub kind: PrivacyFactorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyFactorKind {
    /// Shield: deposits with the entered amount's note value; `None` when fewer than two.
    AmountCrowd {
        deposits: Option<u64>,
    },
    /// Shield: deposits of the token in the tree new deposits enter.
    CurrentTreeCrowd(TreeDeposits),
    /// Shield: the note value's share of the pool balance, a caution from 10%; `None` when the
    /// pool holds none of the token, also a caution.
    PoolShare(Option<PoolShare>),
    /// Unshield: a planned unshield spends a deposit directly, with the other deposits of that
    /// note's value in its tree.
    DirectExit(TreeDeposits),
    /// Unshield: the fewest deposits large enough to have funded a planned unshield.
    FundingCrowd(TreeDeposits),
    /// Unshield: the fewest exits in the last 30 days with a planned unshield's value.
    RecentExits {
        exits_30d: u64,
    },
    /// Unshield: a planned unshield that spends no deposit directly is within 5% of `deposit`,
    /// one of the wallet's own, with `others` other deposits within 5% of it; the match with the
    /// fewest. `others` is a lower bound: the snapshot omits values used only once.
    CloseToOwnDeposit {
        deposit: U256,
        others: u64,
    },
    /// Unshield: the token's exit activity, and whether waiting for more of it helps.
    QuietPeriod {
        exits_24h: u64,
        exits_30d: u64,
        suggest_waiting: bool,
    },
    /// Unshield: the age of the youngest deposit an input note came from.
    RecentDeposit {
        age_secs: u64,
    },
    Sender(UnshieldSender),
    /// Unshield: the recipient is a profile Public account that deposited into the pool.
    RecipientDeposited,
    Network(NetworkRoute),
}

/// Deposits of the token in one commitment tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeDeposits {
    pub tree: u32,
    pub deposits: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnshieldSender {
    /// A Public account of this profile sends the transaction.
    PublicAccount,
    Broadcaster,
    /// The wallet only prepares calldata for the user to submit elsewhere.
    CalldataOnly,
}

/// Where the action's RPC traffic for the chain goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkRoute {
    /// Built-in Tor or the configured proxy.
    Private,
    /// Direct network mode, or an RPC endpoint the user marked direct.
    Direct,
}

/// Why the lines and factors that need the snapshot are missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticsNotice {
    NotConfigured,
    NotYetAvailable,
    OutOfDate,
}

pub struct ShieldHintInput<'a> {
    pub statistics: &'a AmountStatisticsStatus,
    /// The token the pool holds: the wrapped token for a base-token shield.
    pub token: Address,
    pub amount: U256,
    pub shield_fee_bps: U256,
    /// The source's balance of the token: common amounts above it can't be chosen.
    pub balance: U256,
    /// The tree new deposits enter, from the wallet's Merkle forest tip; `None` while the forest
    /// is not synced.
    pub current_tree: Option<u32>,
    pub route: NetworkRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShieldHints {
    /// The entered amount less the shield fee: the value the deposit's note will hold.
    pub note_value: U256,
    /// `Err` when statistics are unavailable for the chain.
    pub lines: Result<ShieldLines, StatisticsNotice>,
    /// Bad first, then caution, then good.
    pub factors: Vec<PrivacyFactor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShieldLines {
    /// Deposits on the chain with the note value; `None` when fewer than two used it.
    pub deposits: Option<u64>,
    /// `None` when the pool holds none of the token.
    pub pool: Option<PoolShare>,
    /// Whether any amount of the token was used twice.
    pub repeated_amounts: bool,
    /// Amounts at least `COMMON_AMOUNT_MIN_DEPOSITS` deposits used, the `COMMON_AMOUNTS_PER_SIDE`
    /// nearest by note value on each side of the entered one, with the entered amount in its
    /// place; ascending. Empty when no such amount other than the entered one exists.
    pub common_amounts: Vec<CommonAmount>,
    /// `None` when the current tree is unknown; zero deposits when the tree holds none.
    pub current_tree: Option<TreeDeposits>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonAmount {
    /// The amount to type: before the shield fee on Shield, the exit's value on Unshield.
    pub amount: U256,
    /// All-time deposits of its note value for Shield's list; exits in the last 30 days with
    /// its value for Unshield's.
    pub count: u64,
    /// Marks the entered amount.
    pub entered: bool,
    /// The amount can be entered in full: the source can shield it, or the Unshield form allows
    /// it.
    pub within_balance: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolShare {
    pub balance: U256,
    /// The note value's share of `balance` in parts per million, rounded down.
    pub share_ppm: u64,
}

pub struct UnshieldHintInput<'a> {
    pub statistics: &'a AmountStatisticsStatus,
    /// The token the pool holds: the wrapped token for a base-token unshield.
    pub token: Address,
    /// From [`resolve_planned_unshields`] for the form's current estimate; `None` while there is
    /// none.
    pub planned: Option<&'a [ResolvedPlannedUnshield]>,
    /// From [`own_shield_deposits`] for `token`.
    pub own_deposits: &'a [OwnDeposit],
    pub sender: UnshieldSender,
    /// From [`recipient_deposited`].
    pub recipient_deposited: bool,
    pub route: NetworkRoute,
    /// Unix seconds.
    pub now: u64,
    /// The entered amount when a broadcaster fee in the unshield's token is taken from it, so
    /// the planned values are that amount less the fee; `None` otherwise.
    pub entered_amount_with_deducted_fee: Option<U256>,
    /// The most the form lets the user enter: common exit amounts above it can't be chosen.
    /// `None` when unknown.
    pub max_amount: Option<U256>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnshieldHints {
    /// `Err` when statistics are unavailable for the chain, `Ok(None)` while there is no plan.
    pub lines: Result<Option<UnshieldLines>, StatisticsNotice>,
    /// Bad first, then caution, then good.
    pub factors: Vec<PrivacyFactor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnshieldLines {
    /// One per planned unshield in batch order. More than one means the exit is sent as that
    /// many unshields.
    pub unshields: Vec<PlannedUnshieldLines>,
    /// The fewest deposits in a planned unshield's input tree large enough to have funded it.
    pub funding_crowd: TreeDeposits,
    /// Values at least `COMMON_EXIT_MIN_EXITS_30D` exits used in the last 30 days, the
    /// `COMMON_AMOUNTS_PER_SIDE` nearest on each side of the single planned unshield's value,
    /// with it in its place; ascending. Empty with more than one planned unshield, or when no
    /// such value other than the entered one exists.
    pub common_exit_amounts: Vec<CommonAmount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedUnshieldLines {
    /// The value the counts are for: the planned unshield's value, with a broadcaster fee taken
    /// from the entered amount added back to the first, which pays it.
    pub value: U256,
    /// Exits in the last 30 days with this value; `None` when fewer than two.
    pub exits_30d: Option<u64>,
    /// Set when this unshield spends a deposit directly: the other deposits with that note's
    /// value in its tree, the wallet's own left out.
    pub direct_exit: Option<TreeDeposits>,
}

/// A planned unshield with its input notes resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlannedUnshield {
    pub tree: u32,
    /// The value leaving the pool as this transaction's unshield.
    pub value: U256,
    pub change: U256,
    pub inputs: Vec<ResolvedPlannedInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPlannedInput {
    pub value: U256,
    /// The wallet received this note from a shield.
    pub is_shield: bool,
    /// Unix seconds of the block that created the note.
    pub source_timestamp: u64,
}

/// One of the wallet's own deposits of a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnDeposit {
    pub tree: u32,
    pub value: U256,
}

#[must_use]
pub fn evaluate_shield_hints(input: &ShieldHintInput<'_>) -> ShieldHints {
    let note_value = shield_note_value(input.amount, input.shield_fee_bps);
    let lines = token_statistics(input.statistics, input.token)
        .map(|stats| shield_lines(stats, note_value, input));
    let mut factors = vec![network_factor(input.route)];
    if let Ok(lines) = &lines {
        factors.extend(shield_factors(lines));
    }
    factors.sort_by_key(|factor| factor.level);
    ShieldHints {
        note_value,
        lines,
        factors,
    }
}

#[must_use]
pub fn evaluate_unshield_hints(input: &UnshieldHintInput<'_>) -> UnshieldHints {
    let mut factors: Vec<PrivacyFactor> = [
        sender_factor(input.sender),
        input.recipient_deposited.then_some(PrivacyFactor {
            level: PrivacyLevel::Bad,
            kind: PrivacyFactorKind::RecipientDeposited,
        }),
        input
            .planned
            .and_then(|planned| recent_deposit_factor(planned, input.now)),
        Some(network_factor(input.route)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let lines = token_statistics(input.statistics, input.token).map(|stats| {
        factors.extend(quiet_period_factor(stats));
        let lines = input
            .planned
            .and_then(|planned| unshield_lines(stats, planned, input));
        if let Some(lines) = &lines {
            factors.extend(unshield_factors(stats, lines, input.own_deposits));
        }
        lines
    });
    factors.sort_by_key(|factor| factor.level);
    UnshieldHints { lines, factors }
}

/// Maps each planned unshield's selected inputs back to `utxos`, the notes the estimate selected
/// from, as the sponsored submission resolves `SelectedInputIdentity` values. `None` when an
/// input is missing, so the hints never describe notes the plan did not pick.
#[must_use]
pub fn resolve_planned_unshields(
    planned: &[PlannedUnshield],
    utxos: &[Utxo],
) -> Option<Vec<ResolvedPlannedUnshield>> {
    let by_identity: BTreeMap<_, _> = utxos
        .iter()
        .map(|utxo| (SelectedInputIdentity::from_utxo(utxo), utxo))
        .collect();
    planned
        .iter()
        .map(|unshield| {
            let inputs = unshield
                .inputs
                .iter()
                .map(|identity| {
                    let utxo = by_identity.get(identity)?;
                    Some(ResolvedPlannedInput {
                        value: utxo.note.value,
                        is_shield: utxo.poi.commitment_kind == UtxoCommitmentKind::Shield,
                        source_timestamp: utxo.source.block_timestamp,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(ResolvedPlannedUnshield {
                tree: unshield.tree,
                value: unshield.value,
                change: unshield.change,
                inputs,
            })
        })
        .collect()
}

/// The wallet's own deposits of `token`: its notes received from a shield, spent or not.
#[must_use]
pub fn own_shield_deposits(notes: &[WalletUtxo], token: Address) -> Vec<OwnDeposit> {
    notes
        .iter()
        .map(|note| &note.utxo)
        .filter(|utxo| {
            utxo.poi.commitment_kind == UtxoCommitmentKind::Shield && utxo.token_address() == token
        })
        .map(|utxo| OwnDeposit {
            tree: utxo.tree,
            value: utxo.note.value,
        })
        .collect()
}

/// Whether `recipient` is the address of one of `accounts` that owns a Shield entry on
/// `chain_id` whose shielding transaction was included and succeeded. Reads stored activity
/// only.
#[must_use]
pub fn recipient_deposited(
    recipient: Address,
    chain_id: u64,
    accounts: &[PublicAccountMetadata],
    entries: &[ActivityEntry],
) -> bool {
    accounts
        .iter()
        .filter(|account| account.address == recipient)
        .any(|account| {
            entries.iter().any(|entry| {
                matches!(&entry.owner, ActivityOwner::Account(uuid) if *uuid == account.public_account_uuid)
                    && shield_succeeded(entry, chain_id)
            })
        })
}

fn shield_succeeded(entry: &ActivityEntry, chain_id: u64) -> bool {
    // An observer can still write an outcome into a cleared stub.
    !entry.cleared
        && entry.kind == ActivityKind::Shield
        && entry.network == ActivityNetwork::Evm(chain_id)
        && entry.attempts.iter().any(|attempt| {
            attempt.step == ActivityStep::Main
                && matches!(
                    attempt.outcome,
                    Some(ActivityAttemptOutcome::Included { success: true, .. })
                )
        })
}

fn token_statistics(
    status: &AmountStatisticsStatus,
    token: Address,
) -> Result<Option<&TokenAmountStatistics>, StatisticsNotice> {
    match status {
        AmountStatisticsStatus::Available(snapshot) => Ok(snapshot.token(token)),
        AmountStatisticsStatus::NotConfigured => Err(StatisticsNotice::NotConfigured),
        AmountStatisticsStatus::NotYetAvailable => Err(StatisticsNotice::NotYetAvailable),
        AmountStatisticsStatus::OutOfDate => Err(StatisticsNotice::OutOfDate),
    }
}

fn shield_lines(
    stats: Option<&TokenAmountStatistics>,
    note_value: U256,
    input: &ShieldHintInput<'_>,
) -> ShieldLines {
    let deposits = deposits_with_value(stats, note_value);
    let rows = stats.map_or(&[][..], |stats| stats.repeated_shield_amounts.as_slice());
    let entered = CommonAmount {
        amount: input.amount,
        count: deposits.unwrap_or(0),
        entered: true,
        within_balance: input.amount <= input.balance,
    };
    ShieldLines {
        deposits,
        pool: stats
            .map(|stats| stats.pool_balance)
            .filter(|balance| !balance.is_zero())
            .map(|balance| PoolShare {
                balance,
                share_ppm: share_ppm(note_value, balance),
            }),
        repeated_amounts: stats.is_some_and(|stats| !stats.repeated_shield_amounts.is_empty()),
        common_amounts: common_amounts_around(shield_candidates(rows, input), note_value, entered),
        current_tree: input.current_tree.map(|tree| TreeDeposits {
            tree,
            deposits: tree_deposits(stats, tree, 0),
        }),
    }
}

fn shield_factors(lines: &ShieldLines) -> impl Iterator<Item = PrivacyFactor> {
    let amount = crowd_factor(
        lines.deposits.unwrap_or(0),
        PrivacyFactorKind::AmountCrowd {
            deposits: lines.deposits,
        },
    );
    let tree = lines
        .current_tree
        .map(|tree| crowd_factor(tree.deposits, PrivacyFactorKind::CurrentTreeCrowd(tree)));
    let pool = PrivacyFactor {
        level: match lines.pool {
            Some(pool) if pool.share_ppm < POOL_SHARE_CAUTION_PPM => PrivacyLevel::Good,
            _ => PrivacyLevel::Caution,
        },
        kind: PrivacyFactorKind::PoolShare(lines.pool),
    };
    [Some(amount), tree, Some(pool)].into_iter().flatten()
}

fn unshield_lines(
    stats: Option<&TokenAmountStatistics>,
    planned: &[ResolvedPlannedUnshield],
    input: &UnshieldHintInput<'_>,
) -> Option<UnshieldLines> {
    let values = lookup_values(planned, input.entered_amount_with_deducted_fee);
    let funding_crowd = planned
        .iter()
        .zip(&values)
        .map(|(unshield, value)| TreeDeposits {
            tree: unshield.tree,
            deposits: tree_deposits(
                stats,
                unshield.tree,
                amount_value_bucket(*value).saturating_add(1),
            ),
        })
        .min_by_key(|crowd| crowd.deposits)?;
    let unshields: Vec<_> = planned
        .iter()
        .zip(values)
        .map(|(unshield, value)| PlannedUnshieldLines {
            value,
            exits_30d: exits_30d_with_value(stats, value),
            direct_exit: direct_exit(stats, unshield, input.own_deposits),
        })
        .collect();
    let common_exit_amounts = match unshields.as_slice() {
        [single] => common_exit_amounts(stats, single, input.max_amount),
        _ => Vec::new(),
    };
    Some(UnshieldLines {
        unshields,
        funding_crowd,
        common_exit_amounts,
    })
}

/// The planned values with a broadcaster fee taken from the entered amount added back to the
/// first, which the fee is charged to, so the counts are for the amount the user typed.
fn lookup_values(planned: &[ResolvedPlannedUnshield], entered: Option<U256>) -> Vec<U256> {
    let mut values: Vec<_> = planned.iter().map(|unshield| unshield.value).collect();
    let planned_sum = values
        .iter()
        .fold(U256::ZERO, |sum, value| sum.saturating_add(*value));
    let fee = entered.map_or(U256::ZERO, |entered| entered.saturating_sub(planned_sum));
    if let Some(first) = values.first_mut() {
        *first = first.saturating_add(fee);
    }
    values
}

/// A transaction spending exactly one note, received from a shield, with change of at most 1% of
/// it. The count is for the note's value, which a fee taken from the note makes differ from the
/// unshield's.
fn direct_exit(
    stats: Option<&TokenAmountStatistics>,
    unshield: &ResolvedPlannedUnshield,
    own_deposits: &[OwnDeposit],
) -> Option<TreeDeposits> {
    let [note] = unshield.inputs.as_slice() else {
        return None;
    };
    let scaled_change = unshield
        .change
        .saturating_mul(U256::from(DIRECT_EXIT_CHANGE_DIVISOR));
    if !note.is_shield || scaled_change > note.value {
        return None;
    }
    let deposits = stats
        .and_then(|stats| {
            stats
                .repeated_shield_amounts
                .iter()
                .find(|row| row.value == note.value)
        })
        .and_then(|row| row.trees.iter().find(|tree| tree.tree == unshield.tree))
        .map_or(0, |tree| tree.all_time);
    let own = own_deposits
        .iter()
        .filter(|own| own.tree == unshield.tree && own.value == note.value)
        .count() as u64;
    Some(TreeDeposits {
        tree: unshield.tree,
        deposits: deposits.saturating_sub(own),
    })
}

fn unshield_factors(
    stats: Option<&TokenAmountStatistics>,
    lines: &UnshieldLines,
    own_deposits: &[OwnDeposit],
) -> impl Iterator<Item = PrivacyFactor> {
    let direct = lines
        .unshields
        .iter()
        .filter_map(|unshield| unshield.direct_exit)
        .min_by_key(|direct| direct.deposits)
        .map(|direct| crowd_factor(direct.deposits, PrivacyFactorKind::DirectExit(direct)));
    let funding = crowd_factor(
        lines.funding_crowd.deposits,
        PrivacyFactorKind::FundingCrowd(lines.funding_crowd),
    );
    let exits_30d = lines
        .unshields
        .iter()
        .map(|unshield| unshield.exits_30d.unwrap_or(0))
        .min()
        .unwrap_or(0);
    let recent_exits = (exits_30d >= RECENT_EXITS_GOOD_MIN).then_some(PrivacyFactor {
        level: PrivacyLevel::Good,
        kind: PrivacyFactorKind::RecentExits { exits_30d },
    });
    let own = close_to_own_deposit(stats, lines, own_deposits).map(|(deposit, others)| {
        crowd_factor(
            others,
            PrivacyFactorKind::CloseToOwnDeposit { deposit, others },
        )
    });
    [direct, Some(funding), recent_exits, own]
        .into_iter()
        .flatten()
}

fn quiet_period_factor(stats: Option<&TokenAmountStatistics>) -> Option<PrivacyFactor> {
    let (exits_24h, exits_30d) = stats.map_or((0, 0), |stats| {
        (stats.unshields_last_24h, stats.unshields_last_30d)
    });
    let busy = exits_30d >= QUIET_BUSY_MIN_EXITS_30D;
    let lull = exits_24h.saturating_mul(QUIET_LULL_FACTOR) < exits_30d;
    (!busy || lull).then_some(PrivacyFactor {
        level: PrivacyLevel::Caution,
        kind: PrivacyFactorKind::QuietPeriod {
            exits_24h,
            exits_30d,
            suggest_waiting: busy,
        },
    })
}

fn recent_deposit_factor(planned: &[ResolvedPlannedUnshield], now: u64) -> Option<PrivacyFactor> {
    planned
        .iter()
        .flat_map(|unshield| &unshield.inputs)
        .filter(|input| input.is_shield)
        .map(|input| now.saturating_sub(input.source_timestamp))
        .filter(|age| *age < RECENT_DEPOSIT_SECS)
        .min()
        .map(|age_secs| PrivacyFactor {
            level: PrivacyLevel::Caution,
            kind: PrivacyFactorKind::RecentDeposit { age_secs },
        })
}

const fn sender_factor(sender: UnshieldSender) -> Option<PrivacyFactor> {
    let level = match sender {
        UnshieldSender::PublicAccount => PrivacyLevel::Bad,
        UnshieldSender::Broadcaster => PrivacyLevel::Good,
        UnshieldSender::CalldataOnly => return None,
    };
    Some(PrivacyFactor {
        level,
        kind: PrivacyFactorKind::Sender(sender),
    })
}

const fn network_factor(route: NetworkRoute) -> PrivacyFactor {
    let level = match route {
        NetworkRoute::Private => PrivacyLevel::Good,
        NetworkRoute::Direct => PrivacyLevel::Caution,
    };
    PrivacyFactor {
        level,
        kind: PrivacyFactorKind::Network(route),
    }
}

const fn crowd_factor(count: u64, kind: PrivacyFactorKind) -> PrivacyFactor {
    PrivacyFactor {
        level: crowd_level(count),
        kind,
    }
}

const fn crowd_level(count: u64) -> PrivacyLevel {
    if count >= CROWD_GOOD_MIN {
        PrivacyLevel::Good
    } else if count <= CROWD_BAD_MAX {
        PrivacyLevel::Bad
    } else {
        PrivacyLevel::Caution
    }
}

/// Deposits with `value` across all trees; `None` when the snapshot has no row for it.
fn deposits_with_value(stats: Option<&TokenAmountStatistics>, value: U256) -> Option<u64> {
    stats?
        .repeated_shield_amounts
        .iter()
        .find(|row| row.value == value)
        .map(|row| saturating_sum(row.trees.iter().map(|tree| tree.all_time)))
}

/// Exits in the last 30 days with `value`; `None` when the snapshot has no row for it.
fn exits_30d_with_value(stats: Option<&TokenAmountStatistics>, value: U256) -> Option<u64> {
    stats?
        .repeated_unshield_amounts
        .iter()
        .find(|row| row.value == value)
        .map(|row| row.last_30d)
}

/// Deposits of the token in `tree` whose value bucket is at least `min_bucket`.
fn tree_deposits(stats: Option<&TokenAmountStatistics>, tree: u32, min_bucket: u16) -> u64 {
    stats
        .and_then(|stats| {
            stats
                .shield_value_buckets
                .iter()
                .find(|row| row.tree == tree)
        })
        .map_or(0, |row| {
            saturating_sum(
                row.buckets
                    .iter()
                    .filter(|bucket| bucket.bit_length >= min_bucket)
                    .map(|bucket| bucket.count),
            )
        })
}

fn saturating_sum(counts: impl Iterator<Item = u64>) -> u64 {
    counts.fold(0, u64::saturating_add)
}

fn shield_note_value(amount: U256, fee_bps: U256) -> U256 {
    amount.saturating_sub(railgun_protocol_fee_amount(amount, fee_bps))
}

/// The amount to type, at most `balance`, that shields to a note of `value`. Every amount from
/// `(value - 1) * D / (D - fee) + 1` to `value * D / (D - fee)` does; the largest is the round
/// amount a depositor typed, so it is preferred when the balance allows.
fn amount_for_note_value(value: U256, fee_bps: U256, balance: U256) -> Option<U256> {
    let kept = FEE_BASIS_POINTS_DENOMINATOR
        .checked_sub(fee_bps)
        .filter(|kept| !kept.is_zero())?;
    let amount = (value.checked_mul(FEE_BASIS_POINTS_DENOMINATOR)? / kept).min(balance);
    (!amount.is_zero() && shield_note_value(amount, fee_bps) == value).then_some(amount)
}

fn share_ppm(part: U256, whole: U256) -> u64 {
    part.checked_mul(U256::from(PARTS_PER_MILLION))
        .map_or(u64::MAX, |scaled| (scaled / whole).saturating_to())
}

mod common_amounts;
mod own_deposits;

#[cfg(test)]
mod tests;
