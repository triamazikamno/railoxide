//! The `hints:` modes: stand-in amount statistics for the Shield and Unshield privacy hints,
//! and the stand-in plan of the Unshield form's estimate. Nothing is fetched or stored.

use std::sync::Arc;

use serde_json::{Value, json};
use wallet_ops::{
    AmountStatisticsStatus, OwnDeposit, ResolvedPlannedInput, ResolvedPlannedUnshield,
};

use super::wallet_screens::stand_in_fee_token;
#[allow(clippy::wildcard_imports)]
use super::*;

const DAY_SECS: u64 = 24 * 60 * 60;

/// Whether a `hints:` mode is active, whose Shield preview opens the stand-in USDC form.
pub(in crate::root) fn hints_active() -> bool {
    matches!(mode(), Some(Mode::Hints(_)))
}

/// The token a `hints:` mode's Shield preview shields on `chain_id`.
pub(in crate::root) fn hints_shield_token(chain_id: u64) -> Address {
    stand_in_fee_token(chain_id).token
}

/// The source balance and current tree a `hints:` mode's Shield form reads in place of the
/// account's and the forest's: 1,500 of a token of `decimals`, and tree 2. `None` in every
/// other mode.
pub(in crate::root) fn shield_hint_stand_ins(decimals: Option<u8>) -> Option<(U256, Option<u32>)> {
    hints_active().then(|| (units(1_500, decimals.unwrap_or(6)), Some(2)))
}

/// The wallet's own deposits a `hints:` mode's Unshield form reads in place of the session's:
/// in `fresh`, one of 0.51 of a token of `decimals` in tree 0, within 5% of the planned 0.5.
/// `None` in every other mode.
pub(in crate::root) fn own_deposit_stand_ins(decimals: Option<u8>) -> Option<Vec<OwnDeposit>> {
    matches!(mode(), Some(Mode::Hints(Hints::Fresh))).then(|| {
        vec![OwnDeposit {
            tree: 0,
            value: units(51, decimals.unwrap_or(18)) / U256::from(100),
        }]
    })
}

/// The amount statistics a `hints:` mode shows on `chain_id`, whose wrapped native token is
/// `wrapped_native`, in place of the session's. `None` in every other mode.
pub(in crate::root) fn amount_statistics(
    chain_id: u64,
    wrapped_native: Option<Address>,
) -> Option<AmountStatisticsStatus> {
    let Mode::Hints(hints) = mode()? else {
        return None;
    };
    if hints == Hints::OutOfDate {
        return Some(AmountStatisticsStatus::OutOfDate);
    }
    let usdc = stand_in_fee_token(chain_id);
    let mut tokens = vec![usdc_statistics(hints, usdc.token, usdc.decimals)];
    tokens.extend(wrapped_native.map(|token| wrapped_native_statistics(hints, token)));
    let snapshot = serde_json::from_value(json!({
        "encoding_version": 1,
        "scope": { "chain_type": "evm", "chain_id": chain_id, "railgun_contract": Address::ZERO },
        "covered_block": { "number": 0, "hash": B256::ZERO, "timestamp": now_unix() },
        "tokens": tokens,
    }))
    .ok()?;
    Some(AmountStatisticsStatus::Available(Arc::new(snapshot)))
}

/// The plan of the stand-in estimate a `hints:` mode gives an Unshield of 0.5 of `asset`: one
/// unshield from tree 0. `direct-exit` spends a 0.5 deposit in full, `out-of-date` half of a
/// deposit made 20 minutes ago, and the others half of a note received in a private transfer.
/// `None` in every other mode.
pub(super) fn stand_in_planned_unshields(
    asset: &crate::root::UnshieldAsset,
) -> Option<Vec<ResolvedPlannedUnshield>> {
    let Mode::Hints(hints) = mode()? else {
        return None;
    };
    let half = units(1, asset.decimals.unwrap_or(18)) / U256::from(2);
    let (note, is_shield, age_secs) = match hints {
        Hints::DirectExit => (half, true, 2 * DAY_SECS),
        Hints::OutOfDate => (half * U256::from(2), true, 20 * 60),
        Hints::Fresh | Hints::NoRepeat => (half * U256::from(2), false, 3 * DAY_SECS),
    };
    Some(vec![ResolvedPlannedUnshield {
        tree: 0,
        value: half,
        change: note - half,
        inputs: vec![ResolvedPlannedInput {
            value: note,
            is_shield,
            source_timestamp: now_unix().saturating_sub(age_secs),
        }],
    }])
}

/// USDC for the Shield form: 1,204 deposits used 1,000 (997.5 after the fee), 437 used 500, 210
/// used 100, 45 used 250, 38 used 800, 32 used 1,200 and 64 used 2,000, which the 1,500 balance
/// can't shield, so the common amounts show it dimmed. Tree 2 holds 7 deposits.
fn usdc_statistics(hints: Hints, token: Address, decimals: u8) -> Value {
    let note = |whole: u64, thousandths: u64| {
        units(whole, decimals) + units(thousandths, decimals) / U256::from(1_000)
    };
    let repeated = if hints == Hints::NoRepeat {
        Vec::new()
    } else {
        vec![
            repeated_shield(note(997, 500), &[(0, 900), (1, 250), (2, 54)]),
            repeated_shield(note(498, 750), &[(1, 300), (2, 137)]),
            repeated_shield(note(1_995, 0), &[(0, 64)]),
            repeated_shield(note(99, 750), &[(0, 150), (1, 60)]),
            repeated_shield(note(249, 375), &[(1, 45)]),
            repeated_shield(note(798, 0), &[(0, 38)]),
            repeated_shield(note(1_197, 0), &[(1, 32)]),
        ]
    };
    token_statistics(
        token,
        units(2_350_000, decimals),
        (40, 1_100),
        &repeated,
        vec![(2, vec![(30, 4), (31, 3)])],
        &[],
    )
}

/// The wrapped native token for the Unshield form, whose 0.5 is in value bucket 59.
/// `direct-exit`: 5 deposits of 0.5, 1 of them in tree 0, 3 recent exits of it, 9 exits in 30
/// days and 12 deposits in tree 0 above its bucket. Otherwise 1,201 deposits of 0.5, 90 recent
/// exits of it, 600 exits in 30 days and 18 in a day, and 412 deposits in tree 0 above its
/// bucket; `no-repeat` lists no repeated amount. Besides 0.5, recent exits used 0.1 210 times,
/// 0.25 45 times, 0.3 31 times, 0.75 60 times, 1 412 times and 2 96 times.
fn wrapped_native_statistics(hints: Hints, token: Address) -> Value {
    let half = units(1, 18) / U256::from(2);
    // (hundredths of a token, exits in the last 30 days)
    let recent_exits = [
        (10, 210),
        (25, 45),
        (30, 31),
        (50, 90),
        (75, 60),
        (100, 412),
        (200, 96),
    ]
    .map(|(hundredths, last_30d)| {
        json!({ "value": units(hundredths, 18) / U256::from(100), "last_30d": last_30d })
    });
    let (repeated, exits, unshields, funding) = match hints {
        Hints::DirectExit => (
            vec![repeated_shield(half, &[(0, 1), (3, 4)])],
            vec![json!({ "value": half, "last_30d": 3 })],
            (1, 9),
            vec![(60, 12)],
        ),
        Hints::NoRepeat => (
            Vec::new(),
            Vec::new(),
            (18, 600),
            vec![(60, 300), (61, 112)],
        ),
        Hints::Fresh | Hints::OutOfDate => (
            vec![repeated_shield(half, &[(0, 812), (1, 389)])],
            recent_exits.to_vec(),
            (18, 600),
            vec![(60, 300), (61, 112)],
        ),
    };
    let mut buckets = vec![(59, 40)];
    buckets.extend(funding);
    token_statistics(
        token,
        units(8_400, 18),
        unshields,
        &repeated,
        vec![(0, buckets)],
        &exits,
    )
}

fn repeated_shield(value: U256, trees: &[(u32, u64)]) -> Value {
    json!({
        "value": value,
        "trees": trees
            .iter()
            .map(|(tree, all_time)| json!({ "tree": tree, "all_time": all_time, "last_30d": 0 }))
            .collect::<Vec<_>>(),
    })
}

/// `unshields` are the exits in the last 24 hours and 30 days, and `buckets` each tree's
/// deposits by value bucket.
fn token_statistics(
    token: Address,
    pool_balance: U256,
    unshields: (u64, u64),
    repeated_shield_amounts: &[Value],
    buckets: Vec<(u32, Vec<(u16, u64)>)>,
    repeated_unshield_amounts: &[Value],
) -> Value {
    json!({
        "token": token,
        "pool_balance": pool_balance,
        "shields_last_24h": 0,
        "shields_last_30d": 0,
        "unshields_last_24h": unshields.0,
        "unshields_last_30d": unshields.1,
        "repeated_shield_amounts": repeated_shield_amounts,
        "shield_value_buckets": buckets
            .into_iter()
            .map(|(tree, counts)| json!({
                "tree": tree,
                "buckets": counts
                    .into_iter()
                    .map(|(bit_length, count)| json!({ "bit_length": bit_length, "count": count }))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "repeated_unshield_amounts": repeated_unshield_amounts,
    })
}

/// `whole` tokens of `decimals`.
fn units(whole: u64, decimals: u8) -> U256 {
    U256::from(whole) * U256::from(10_u8).pow(U256::from(decimals))
}
