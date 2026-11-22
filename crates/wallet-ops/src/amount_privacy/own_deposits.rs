//! Whether an exit is close to one of the wallet's own deposits.

use alloy::primitives::U256;
use sync_service::TokenAmountStatistics;

use super::{OWN_DEPOSIT_MATCH_DIVISOR, OwnDeposit, UnshieldLines, saturating_sum};

/// The own deposit that a planned unshield spending no deposit directly is within 1/20 of, with
/// the other deposits within 1/20 of it; across all matches, the one with the fewest.
pub(super) fn close_to_own_deposit(
    stats: Option<&TokenAmountStatistics>,
    lines: &UnshieldLines,
    own_deposits: &[OwnDeposit],
) -> Option<(U256, u64)> {
    lines
        .unshields
        .iter()
        .filter(|unshield| unshield.direct_exit.is_none())
        .flat_map(|unshield| {
            own_deposits
                .iter()
                .filter(move |own| close_to_deposit(unshield.value, own.value))
        })
        .map(|own| (own.value, deposits_near(stats, own.value, own_deposits)))
        .min_by_key(|(_, others)| *others)
}

/// Deposits whose value is within 1/20 of `deposit`, less the wallet's own of each such value. A
/// lower bound: the snapshot omits values used only once.
fn deposits_near(
    stats: Option<&TokenAmountStatistics>,
    deposit: U256,
    own_deposits: &[OwnDeposit],
) -> u64 {
    let rows = stats.map_or(&[][..], |stats| stats.repeated_shield_amounts.as_slice());
    saturating_sum(
        rows.iter()
            .filter(|row| close_to_deposit(row.value, deposit))
            .map(|row| {
                let own = own_deposits
                    .iter()
                    .filter(|own| own.value == row.value)
                    .count() as u64;
                saturating_sum(row.trees.iter().map(|tree| tree.all_time)).saturating_sub(own)
            }),
    )
}

/// `|value - deposit| * 20 <= deposit`.
fn close_to_deposit(value: U256, deposit: U256) -> bool {
    value
        .abs_diff(deposit)
        .saturating_mul(U256::from(OWN_DEPOSIT_MATCH_DIVISOR))
        <= deposit
}
