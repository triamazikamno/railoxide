//! The common amounts lists: the nearby amounts many deposits, or recent exits, used.

use alloy::primitives::U256;
use sync_service::{RepeatedShieldAmount, TokenAmountStatistics};

use super::{
    COMMON_AMOUNT_MIN_DEPOSITS, COMMON_AMOUNTS_PER_SIDE, COMMON_EXIT_MIN_EXITS_30D, CommonAmount,
    PlannedUnshieldLines, ShieldHintInput, amount_for_note_value, saturating_sum,
};

/// Of `candidates`, each paired with the value it is compared by, the `COMMON_AMOUNTS_PER_SIDE`
/// nearest on each side of `value`, the entered one's, with `entered` in its place, ascending;
/// empty when no candidate other than one with `value` exists.
pub(super) fn common_amounts_around(
    candidates: impl Iterator<Item = (U256, CommonAmount)>,
    value: U256,
    entered: CommonAmount,
) -> Vec<CommonAmount> {
    // A candidate with the entered value is the entered one.
    let (mut below, mut above): (Vec<_>, Vec<_>) = candidates
        .filter(|(candidate, _)| *candidate != value)
        .partition(|(candidate, _)| *candidate < value);
    if below.is_empty() && above.is_empty() {
        return Vec::new();
    }
    below.sort_unstable_by_key(|(candidate, _)| *candidate);
    above.sort_unstable_by_key(|(candidate, _)| *candidate);
    below[below.len().saturating_sub(COMMON_AMOUNTS_PER_SIDE)..]
        .iter()
        .map(|(_, common)| *common)
        .chain(std::iter::once(entered))
        .chain(
            above
                .iter()
                .take(COMMON_AMOUNTS_PER_SIDE)
                .map(|(_, common)| *common),
        )
        .collect()
}

/// The repeated amounts at least `COMMON_AMOUNT_MIN_DEPOSITS` deposits used, each with its note
/// value. One the balance can't cover shows the amount a depositor typed.
pub(super) fn shield_candidates(
    rows: &[RepeatedShieldAmount],
    input: &ShieldHintInput<'_>,
) -> impl Iterator<Item = (U256, CommonAmount)> {
    let (fee_bps, balance) = (input.shield_fee_bps, input.balance);
    rows.iter().filter_map(move |row| {
        let deposits = saturating_sum(row.trees.iter().map(|tree| tree.all_time));
        if deposits < COMMON_AMOUNT_MIN_DEPOSITS {
            return None;
        }
        let within_balance = amount_for_note_value(row.value, fee_bps, balance);
        let amount =
            within_balance.or_else(|| amount_for_note_value(row.value, fee_bps, U256::MAX))?;
        let common = CommonAmount {
            amount,
            count: deposits,
            entered: false,
            within_balance: within_balance.is_some(),
        };
        Some((row.value, common))
    })
}

/// The common exit amounts around `unshield`'s value: values with at least
/// `COMMON_EXIT_MIN_EXITS_30D` exits in the last 30 days. One above `max_amount` can't be chosen.
pub(super) fn common_exit_amounts(
    stats: Option<&TokenAmountStatistics>,
    unshield: &PlannedUnshieldLines,
    max_amount: Option<U256>,
) -> Vec<CommonAmount> {
    let allowed = |value: U256| max_amount.is_none_or(|max| value <= max);
    let rows = stats.map_or(&[][..], |stats| stats.repeated_unshield_amounts.as_slice());
    let candidates = rows
        .iter()
        .filter(|row| row.last_30d >= COMMON_EXIT_MIN_EXITS_30D)
        .map(|row| {
            let common = CommonAmount {
                amount: row.value,
                count: row.last_30d,
                entered: false,
                within_balance: allowed(row.value),
            };
            (row.value, common)
        });
    let entered = CommonAmount {
        amount: unshield.value,
        count: unshield.exits_30d.unwrap_or(0),
        entered: true,
        within_balance: allowed(unshield.value),
    };
    common_amounts_around(candidates, unshield.value, entered)
}
