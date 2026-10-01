//! Gas estimate and order limit for a private swap.
//!
//! The order limit starts from the quote's best case: `CoW`'s quoted buy amount with its network
//! fee added back at the quoted trading rate. The wallet prices one gas estimate: the quote's
//! swap gas units plus the conservative gas of the order's hooks, at the caller's cushioned RPC
//! gas price, plus any rollup data cost. `CoW`'s quoted gas price never enters it. The user's
//! gas share of that estimate, the price tolerance on the best case, and for Private delivery
//! the shield fee are deducted to give the minimum. Solvers may charge less gas than the share
//! allows, and the surplus still reaches the user. The declared gas limit only caps execution
//! and is not priced. The hook gas estimate depends only on the chain and transaction shapes.
//! It reads no chain state and sends nothing to an RPC, so it works before delegation and never
//! exposes a signed hook.
//!
//! # Calibration
//!
//! Hook gas here is what a hook adds to a settlement: the hook call's own gas, the trampoline's
//! call overhead, and the hook calldata. It errs high on purpose. A high estimate only lowers the
//! guaranteed minimum, and the surplus still reaches the user. A low one makes the order
//! unattractive to solvers.
//!
//! The Railgun parts come from the shared per-chain model, [`RailgunGasModel`], in its
//! [`GasEstimateMode::UpperBound`] mode for the gas estimate and signing limits: the
//! `RelayAdapt7702` execute ([`RailgunGasModel::executor`]), the pre-hook's `transact`, and the
//! post-hook's one-leaf shield. Its docs hold the calibration and the per-chain samples. Only the calls a hook adds
//! around them are measured here, on anvil 1.7.1 mainnet forks at blocks 26,060,041, 26,060,057
//! and 26,060,078 with the executor delegated to `RelayAdapt7702` `0x05ae…d963`, running through
//! a copy of `HooksTrampoline` with fresh executors so every slot starts cold. They don't depend
//! on the chain's precompile pricing, so every chain uses them:
//!
//! - [`HOOK_PRE_CALLS_GAS`]: the deadline guard, the first exact approval, and the trampoline's
//!   call overhead (6,681 to 7,720 measured). An `execute` with no Railgun transactions measured
//!   90,940 including the executor call.
//! - [`INVALIDATE_ORDER_GAS`]: `GPv2Settlement.invalidateOrder`, measured 33,585.
//! - [`HOOK_GUARD_GAS`]: the post-hook's self-transfer guard and the trampoline's call overhead.
//!   Whole post-hooks measured 803,508 to 820,094 across tokens at the tree's position at the
//!   time, and 855,936 at leaf index 32,768.
//!
//! The first leaf of a fresh tree costs more than the upper bound, up to 902,453 for a post-hook
//! (`cbBTC` with Railgun and treasury balances starting at zero). That happens once per 65,536
//! leaves, so the declared limit's margin covers it rather than every order paying for it.

use alloy::primitives::{Address, U256, uint};
use broadcaster_core::contracts::shield::{ShieldFeeError, min_shield_amount};
use railgun_wallet::tx::{GasEstimateMode, RailgunGasModel, TransactionShape};

use super::{CowQuote, CowQuoteParameters};
use crate::{FEE_BASIS_POINTS_DENOMINATOR, railgun_protocol_fee_amount};

/// The pre-hook's deadline guard, first exact approval, and trampoline call overhead.
const HOOK_PRE_CALLS_GAS: u64 = 40_000;
/// `GPv2Settlement.invalidateOrder` in the pre-hook. Measured 33,585 on a mainnet fork.
const INVALIDATE_ORDER_GAS: u64 = 35_000;
/// The post-hook's self-transfer guard and trampoline call overhead.
const HOOK_GUARD_GAS: u64 = 25_000;
/// The Across post-hook's exact approval of the `SpokePool`. Measured at most 25,603 inside the
/// post-hook on forks of all four chains.
const ACROSS_APPROVE_GAS: u64 = 30_000;
/// The Across post-hook's `depositV3`. Measured at most 42,339 on forks of all four chains; a
/// `SpokePool` that holds none of the token adds about 17,100.
const ACROSS_DEPOSIT_GAS: u64 = 60_000;

const WEI_PER_NATIVE: U256 = uint!(1_000_000_000_000_000_000_U256);

/// The Tight preset: the order's minimum deducts 10% of the gas estimate.
pub const GAS_SHARE_TIGHT_BPS: u16 = 1_000;
/// The Balanced preset and a new swap's default: 25% of the gas estimate.
pub const GAS_SHARE_BALANCED_BPS: u16 = 2_500;
/// The Loose preset: the whole gas estimate.
pub const GAS_SHARE_LOOSE_BPS: u16 = 10_000;

/// Calls a pre-hook makes besides the deadline guard and the exact approval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreHookCalls {
    /// Invalidates the executor's still-live earlier order (design decision 3).
    pub invalidate_order: bool,
}

/// Estimated gas of a pre-hook `execute` that runs `transactions` and `calls`.
#[must_use]
pub fn pre_hook_gas(
    model: &RailgunGasModel,
    transactions: &[TransactionShape],
    calls: PreHookCalls,
    mode: GasEstimateMode,
) -> u64 {
    let invalidate = if calls.invalidate_order {
        INVALIDATE_ORDER_GAS
    } else {
        0
    };
    model
        .executor()
        .saturating_add(HOOK_PRE_CALLS_GAS)
        .saturating_add(invalidate)
        .saturating_add(model.transact(mode, transactions.into()))
}

/// Estimated gas of the post-hook `multicall`: the guard and a full-balance shield of one leaf.
#[must_use]
pub const fn post_hook_gas(model: &RailgunGasModel, mode: GasEstimateMode) -> u64 {
    model
        .executor()
        .saturating_add(HOOK_GUARD_GAS)
        .saturating_add(model.shield(mode, 1))
}

/// Estimated gas of the Across post-hook `multicall`: the guard, the approval of the
/// `SpokePool` and the deposit, and with `reshield_surplus` a full-balance shield of one leaf.
#[must_use]
pub const fn across_post_hook_gas(
    model: &RailgunGasModel,
    reshield_surplus: bool,
    mode: GasEstimateMode,
) -> u64 {
    let calls = model
        .executor()
        .saturating_add(HOOK_GUARD_GAS)
        .saturating_add(ACROSS_APPROVE_GAS)
        .saturating_add(ACROSS_DEPOSIT_GAS);
    if reshield_surplus {
        calls.saturating_add(model.shield(mode, 1))
    } else {
        calls
    }
}

/// Gas limit to declare for a hook in the app data: the estimate plus 10%. The margin covers
/// the 1/64 of gas a call keeps back from the calls it makes, and variation the measurements
/// didn't cover.
#[must_use]
pub const fn hook_gas_limit(estimate: u64) -> u64 {
    estimate.saturating_add(estimate / 10)
}

/// How to convert native gas cost into the buy token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeBuyRate {
    /// Buy-token base units per whole native token, from a fresh anchor's `buy_rate`.
    Anchor(U256),
    /// For a pair without anchors, whose unverified price the user has acknowledged. The rate
    /// comes from the quote: `CoW`'s `sellTokenPrice`, in wei per sell-token base unit, and the
    /// quoted buy-per-sell ratio. The wallet uses this price anyway, so it adds no new trust.
    Quote,
}

/// Inputs to [`price_order_limit`].
#[derive(Debug, Clone, Copy)]
pub struct OrderLimitParams<'a> {
    /// A hook-free sell quote. For an order with `feeAmount = 0` selling
    /// `sellAmount + feeAmount`, `buyAmount` is the expected output after `CoW`'s network and
    /// protocol fees.
    pub quote: &'a CowQuoteParameters,
    /// The quote's swap gas units, from [`quote_gas_units`].
    pub quote_gas_units: u64,
    /// Sum of the conservative gas estimates of the order's hooks, in
    /// [`GasEstimateMode::UpperBound`] mode and without the declared limits' margin: the
    /// pre-hook, and any post-hook.
    pub hook_gas: u64,
    /// RPC gas price in wei, including the cushion selected by the caller. It prices both the
    /// swap gas and the hook gas. The quote's gas price is never read.
    pub gas_price_wei: u128,
    /// Additional native cost for posting the hooks' calldata on rollups.
    pub hook_data_cost_wei: U256,
    pub native_rate: NativeBuyRate,
    /// Price tolerance applied to the best case, in basis points.
    pub price_tolerance_bps: u32,
    /// Share of the gas estimate the minimum deducts, in basis points of 10,000.
    pub gas_share_bps: u16,
    /// Zero for an order that pays an External receiver and so carries no shield.
    pub shield_fee_bps: U256,
}

/// The order limit for a quote. Amounts are in buy-token base units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderLimit {
    /// The quoted buy amount with `CoW`'s network fee added back at the quoted trading rate.
    pub best_case: U256,
    /// The gas estimate: swap and hook gas at the cushioned RPC price, plus rollup data cost.
    pub gas_estimate: U256,
    /// The share of `gas_estimate` the minimum deducts, rounded up.
    pub gas_allowance: U256,
    /// Suggested minimum received privately after the shield fee, or by an External receiver.
    pub min_received: U256,
    /// The order's `buyAmount` for `min_received`.
    pub buy_amount: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OrderLimitError {
    /// The allowed gas leaves no positive minimum.
    #[error("gas costs at least the quoted output")]
    HookCostExceedsOutput {
        buy_token: Address,
        /// The gas estimate in buy-token base units.
        gas_estimate: U256,
        /// The quote's best case, in buy-token base units.
        best_case: U256,
    },
    #[error("nothing is left to receive after the price tolerance and the shield fee")]
    NothingToReceive,
    #[error("the price tolerance must be below 10000 basis points")]
    InvalidPriceTolerance,
    #[error("the gas share must be at most 10000 basis points")]
    InvalidGasShare,
    #[error("the quote's native price can't be used")]
    InvalidQuotePrice,
    #[error("the quote's gas amount can't be used")]
    InvalidQuoteGas,
    #[error(transparent)]
    ShieldFee(#[from] ShieldFeeError),
    #[error("order amounts overflow")]
    Overflow,
}

/// Prices the gas share, the price tolerance, and the shield fee into the order limit.
///
/// With the best case `best = buyAmount + floor(feeAmount * buyAmount / sellAmount)`, the gas
/// estimate `G = ceil(((swap_gas + hook_gas) * gas_price + data_cost) * rate / 1e18)` and the
/// allowance `A = ceil(G * share / 10000)`, the suggested minimum is
/// `M = net(floor(best * (10000 - tolerance) / 10000) - A)`, where `net` is Railgun's inclusive
/// shield fee deduction, `x - floor(x * fee / 10000)`. A nonpositive amount before `net` is
/// [`OrderLimitError::HookCostExceedsOutput`]. The order's `buyAmount` is [`order_buy_amount`]
/// of `M`. Every rounding lowers `M`:
/// the gas estimate, the allowance and the quote-derived rate round up, and the network fee and
/// the tolerance round down. `M` is thus never more than the quote supports, and `buyAmount`
/// never exceeds the amount delivered after the tolerance and the allowance.
pub fn price_order_limit(params: &OrderLimitParams<'_>) -> Result<OrderLimit, OrderLimitError> {
    let tolerance = U256::from(params.price_tolerance_bps);
    if tolerance >= FEE_BASIS_POINTS_DENOMINATOR {
        return Err(OrderLimitError::InvalidPriceTolerance);
    }
    let share = U256::from(params.gas_share_bps);
    if share > FEE_BASIS_POINTS_DENOMINATOR {
        return Err(OrderLimitError::InvalidGasShare);
    }
    if params.shield_fee_bps >= FEE_BASIS_POINTS_DENOMINATOR {
        return Err(ShieldFeeError::FeeTooHigh.into());
    }
    let quote = params.quote;
    if quote.sell_amount.is_zero() {
        return Err(OrderLimitError::InvalidQuotePrice);
    }
    let rate = match params.native_rate {
        NativeBuyRate::Anchor(rate) => rate,
        NativeBuyRate::Quote => quote_native_to_buy_rate(quote)?,
    };
    let gas_wei = (U256::from(params.quote_gas_units) + U256::from(params.hook_gas))
        .checked_mul(U256::from(params.gas_price_wei))
        .and_then(|cost| cost.checked_add(params.hook_data_cost_wei))
        .ok_or(OrderLimitError::Overflow)?;
    let gas_estimate = gas_wei
        .checked_mul(rate)
        .ok_or(OrderLimitError::Overflow)?
        .div_ceil(WEI_PER_NATIVE);
    let network_fee = quote
        .fee_amount
        .checked_mul(quote.buy_amount)
        .ok_or(OrderLimitError::Overflow)?
        / quote.sell_amount;
    let best_case = quote
        .buy_amount
        .checked_add(network_fee)
        .ok_or(OrderLimitError::Overflow)?;
    let gas_allowance = gas_estimate
        .checked_mul(share)
        .ok_or(OrderLimitError::Overflow)?
        .div_ceil(FEE_BASIS_POINTS_DENOMINATOR);
    let tolerated = best_case
        .checked_mul(FEE_BASIS_POINTS_DENOMINATOR - tolerance)
        .ok_or(OrderLimitError::Overflow)?
        / FEE_BASIS_POINTS_DENOMINATOR;
    let pre_fee = tolerated
        .checked_sub(gas_allowance)
        .filter(|amount| !amount.is_zero())
        .ok_or(OrderLimitError::HookCostExceedsOutput {
            buy_token: quote.buy_token,
            gas_estimate,
            best_case,
        })?;
    let min_received = pre_fee - railgun_protocol_fee_amount(pre_fee, params.shield_fee_bps);
    Ok(OrderLimit {
        best_case,
        gas_estimate,
        gas_allowance,
        min_received,
        buy_amount: order_buy_amount(min_received, params.shield_fee_bps)?,
    })
}

/// The quote's swap gas units from its `gasAmount`, rounded up to a whole unit.
pub fn quote_gas_units(quote: &CowQuoteParameters) -> Result<u64, OrderLimitError> {
    let (mantissa, exponent) =
        parse_decimal(&quote.gas_amount).ok_or(OrderLimitError::InvalidQuoteGas)?;
    let power = U256::from(10_u8).checked_pow(U256::from(exponent.unsigned_abs()));
    let units = if exponent >= 0 {
        power.and_then(|power| mantissa.checked_mul(power))
    } else {
        // A divisor beyond `U256` leaves less than one unit, which rounds up to one.
        Some(power.map_or_else(
            || {
                if mantissa.is_zero() {
                    U256::ZERO
                } else {
                    U256::ONE
                }
            },
            |power| mantissa.div_ceil(power),
        ))
    };
    units
        .and_then(|units| u64::try_from(units).ok())
        .ok_or(OrderLimitError::InvalidQuoteGas)
}

/// `CoW`'s protocol fee for display, in buy-token base units. A sell quote's `buyAmount` is
/// already net of it, so it is `floor(buyAmount * bps / (10000 - bps))` for the quote's
/// possibly fractional `protocolFeeBps`. `None` when the quote omits the fee or states one
/// that can't be used.
#[must_use]
pub fn quote_protocol_fee(quote: &CowQuote) -> Option<U256> {
    let (mantissa, exponent) = parse_decimal(quote.protocol_fee_bps.as_deref()?)?;
    let power = U256::from(10_u8).checked_pow(U256::from(exponent.unsigned_abs()))?;
    // bps = numerator / scale, in integers.
    let (numerator, scale) = if exponent >= 0 {
        (mantissa.checked_mul(power)?, U256::ONE)
    } else {
        (mantissa, power)
    };
    let denominator = FEE_BASIS_POINTS_DENOMINATOR
        .checked_mul(scale)?
        .checked_sub(numerator)
        .filter(|denominator| !denominator.is_zero())?;
    Some(quote.quote.buy_amount.checked_mul(numerator)? / denominator)
}

/// The order's `buyAmount` for a minimum received privately: the smallest amount whose net
/// after the shield fee is at least `min_received`.
pub fn order_buy_amount(min_received: U256, shield_fee_bps: U256) -> Result<U256, OrderLimitError> {
    min_shield_amount(min_received, shield_fee_bps).map_err(|error| match error {
        ShieldFeeError::ZeroMinimum => OrderLimitError::NothingToReceive,
        other => other.into(),
    })
}

/// Buy-token base units per whole native token implied by a quote, rounded up:
/// `1e18 * buyAmount / (sellTokenPrice * sellAmount)`.
fn quote_native_to_buy_rate(quote: &CowQuoteParameters) -> Result<U256, OrderLimitError> {
    let (price, exponent) =
        parse_decimal(&quote.sell_token_price).ok_or(OrderLimitError::InvalidQuotePrice)?;
    if price.is_zero() || quote.sell_amount.is_zero() {
        return Err(OrderLimitError::InvalidQuotePrice);
    }
    // 1e18 / (price * 10^exponent) = 10^(18 - exponent) / price.
    let scale = 18_i32
        .checked_sub(exponent)
        .ok_or(OrderLimitError::InvalidQuotePrice)?;
    let power = U256::from(10_u8)
        .checked_pow(U256::from(scale.unsigned_abs()))
        .ok_or(OrderLimitError::InvalidQuotePrice)?;
    let (numerator, denominator) = if scale >= 0 {
        (
            quote.buy_amount.checked_mul(power),
            price.checked_mul(quote.sell_amount),
        )
    } else {
        (
            Some(quote.buy_amount),
            price
                .checked_mul(quote.sell_amount)
                .and_then(|product| product.checked_mul(power)),
        )
    };
    let (Some(numerator), Some(denominator)) = (numerator, denominator) else {
        return Err(OrderLimitError::InvalidQuotePrice);
    };
    Ok(numerator.div_ceil(denominator))
}

/// Parses a non-negative decimal such as `412345678.9` or `4.1E-7` into
/// `(mantissa, exponent)` with value `mantissa * 10^exponent`.
fn parse_decimal(text: &str) -> Option<(U256, i32)> {
    let (number, exponent) = match text.split_once(['e', 'E']) {
        Some((number, exponent)) => (number, exponent.parse::<i32>().ok()?),
        None => (text, 0),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits = format!("{whole}{fraction}");
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mantissa = U256::from_str_radix(&digits, 10).ok()?;
    let exponent = exponent.checked_sub(i32::try_from(fraction.len()).ok()?)?;
    Some((mantissa, exponent))
}

#[cfg(test)]
mod tests {
    use railgun_wallet::tx::{ETHEREUM_GAS_MODEL, POLYGON_GAS_MODEL};

    use super::*;

    const fn shape(
        input_count: usize,
        output_count: usize,
        has_unshield: bool,
    ) -> TransactionShape {
        TransactionShape {
            input_count,
            output_count,
            has_unshield,
        }
    }

    fn quote(sell_amount: u64, buy_amount: u64, sell_token_price: &str) -> CowQuoteParameters {
        CowQuoteParameters {
            sell_token: Address::ZERO,
            buy_token: Address::ZERO,
            receiver: None,
            sell_amount: U256::from(sell_amount),
            buy_amount: U256::from(buy_amount),
            valid_to: 0,
            fee_amount: U256::ZERO,
            gas_amount: "0".to_owned(),
            gas_price: "0".to_owned(),
            sell_token_price: sell_token_price.to_owned(),
            kind: "sell".to_owned(),
            partially_fillable: false,
        }
    }

    /// Measured gas from the fork runs in the module docs. Fork rows are
    /// `(transactions of (inputs, outputs), invalidates order, measured gas)`, all with an
    /// unshield to the executor.
    #[test]
    fn estimate_covers_measured_hook_gas() {
        type ForkRow = (&'static [(usize, usize)], bool, u64);
        let fork: &[ForkRow] = &[
            (&[], false, 90_940),
            (&[], true, 124_525),
            (&[(1, 1)], false, 534_326),
            (&[(1, 2)], false, 1_173_858),
            (&[(1, 2)], false, 1_233_418),
            (&[(1, 2)], false, 1_200_969),
            (&[(1, 2)], true, 1_159_565),
            (&[(12, 2)], false, 1_653_804),
            (&[(13, 1)], false, 1_001_860),
            (&[(1, 2), (1, 2)], false, 2_029_134),
            (&[(12, 2), (1, 2)], false, 2_511_882),
            (&[(12, 2), (1, 2), (1, 1)], true, 2_952_758),
            (&[(12, 2), (12, 2)], true, 2_851_133),
            (&[(12, 2), (12, 2), (12, 2)], true, 3_592_500),
            (&[(1, 2); 8], true, 4_460_207),
            (&[(12, 2); 8], true, 7_441_501),
            (&[(13, 1); 8], false, 6_252_944),
        ];
        for (transactions, invalidate_order, measured) in fork {
            let shapes: Vec<_> = transactions
                .iter()
                .map(|(inputs, outputs)| shape(*inputs, *outputs, true))
                .collect();
            let calls = PreHookCalls {
                invalidate_order: *invalidate_order,
            };
            assert!(
                pre_hook_gas(
                    &ETHEREUM_GAS_MODEL,
                    &shapes,
                    calls,
                    GasEstimateMode::UpperBound
                ) >= *measured,
                "{transactions:?} {invalidate_order}"
            );
        }
        // Post-hooks at the tree's position at the time, and at leaf index 32,768.
        for measured in [803_508, 820_094, 855_936] {
            assert!(post_hook_gas(&ETHEREUM_GAS_MODEL, GasEstimateMode::UpperBound) >= measured);
        }
        // The first leaf of a fresh tree is not priced, but the declared limit must let it run.
        for measured in [902_453, 885_487, 878_423] {
            assert!(
                hook_gas_limit(post_hook_gas(
                    &ETHEREUM_GAS_MODEL,
                    GasEstimateMode::UpperBound
                )) >= measured
            );
        }
        // The heaviest Polygon `RelayAdapt7702` shield-only call.
        assert!(post_hook_gas(&POLYGON_GAS_MODEL, GasEstimateMode::UpperBound) >= 936_417);
    }

    /// The spec's Balanced example with the other presets' extremes: a best case of 9.9586
    /// USDT, a 0.1% tolerance, a 5.90 USDT gas estimate from 200,000 swap and 1,800,000 hook gas
    /// at 1 gwei and 2,950 USDT per ETH, and a 25 bp shield fee.
    #[test]
    fn gas_share_deducts_its_part_of_one_gas_estimate() {
        let mut quote = quote(1_000_000, 9_958_600, "1");
        quote.gas_amount = "200000".to_owned();
        quote.gas_price = "1339699137".to_owned();
        let params = OrderLimitParams {
            quote: &quote,
            quote_gas_units: quote_gas_units(&quote).unwrap(),
            hook_gas: 1_800_000,
            gas_price_wei: 1_000_000_000,
            hook_data_cost_wei: U256::ZERO,
            native_rate: NativeBuyRate::Anchor(U256::from(2_950_000_000_u64)),
            price_tolerance_bps: 10,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            shield_fee_bps: U256::from(25),
        };
        // 9.9586 * 0.999 = 9.948641 before any gas; 25% of 5.90 USDT is 1.475 USDT.
        for (share, allowance, pre_fee, received) in [
            (0, 0_u64, 9_948_641_u64, 9_923_770_u64),
            (GAS_SHARE_BALANCED_BPS, 1_475_000, 8_473_641, 8_452_457),
            (GAS_SHARE_LOOSE_BPS, 5_900_000, 4_048_641, 4_038_520),
        ] {
            let limit = price_order_limit(&OrderLimitParams {
                gas_share_bps: share,
                ..params
            })
            .unwrap();
            assert_eq!(
                (limit.best_case, limit.gas_estimate, limit.gas_allowance),
                (
                    U256::from(9_958_600),
                    U256::from(5_900_000),
                    U256::from(allowance)
                ),
                "{share}"
            );
            assert_eq!(limit.min_received, U256::from(received), "{share}");
            assert_eq!(limit.buy_amount, U256::from(pre_fee), "{share}");
        }

        // The quote's gas price is informational, even if it cannot be parsed.
        let limit = price_order_limit(&params).unwrap();
        for cow_price in ["1", "1e100", "NaN"] {
            let mut quote = quote.clone();
            quote.gas_price = cow_price.into();
            assert_eq!(
                price_order_limit(&OrderLimitParams {
                    quote: &quote,
                    ..params
                }),
                Ok(limit),
                "{cow_price}"
            );
        }
        assert_eq!(
            price_order_limit(&OrderLimitParams {
                gas_share_bps: GAS_SHARE_LOOSE_BPS + 1,
                ..params
            }),
            Err(OrderLimitError::InvalidGasShare)
        );
    }

    /// Without a network fee, swap gas or tolerance, Loose deducts the whole hook cost like the
    /// minimum before gas shares: `net(buyAmount - ceil(hook_cost))`.
    #[test]
    fn loose_share_matches_the_minimum_before_gas_shares() {
        // 1,000 USDC quoted; 2M gas at 1 gwei costs 6 USDC at 3,000 USDC/native, and a rollup's
        // additional 0.001 native data cost 3 USDC more.
        let quote = quote(1, 1_000_000_000, "1");
        for (data_cost, hook_cost) in [(0_u64, 6_000_000_u64), (1_000_000_000_000_000, 9_000_000)] {
            let limit = price_order_limit(&OrderLimitParams {
                quote: &quote,
                quote_gas_units: 0,
                hook_gas: 2_000_000,
                gas_price_wei: 1_000_000_000,
                hook_data_cost_wei: U256::from(data_cost),
                native_rate: NativeBuyRate::Anchor(U256::from(3_000_000_000_u64)),
                price_tolerance_bps: 0,
                gas_share_bps: GAS_SHARE_LOOSE_BPS,
                shield_fee_bps: U256::from(25),
            })
            .unwrap();
            let delivered = quote.buy_amount - U256::from(hook_cost);
            let previous = delivered - delivered * U256::from(25) / U256::from(10_000);
            assert_eq!(limit.gas_estimate, U256::from(hook_cost));
            assert_eq!(limit.gas_allowance, limit.gas_estimate);
            assert_eq!(limit.min_received, previous);
            assert_eq!(
                limit.buy_amount,
                min_shield_amount(previous, U256::from(25)).unwrap()
            );
        }
    }

    #[test]
    fn rounding_never_overstates_the_minimum() {
        // 1 wei of gas cost rounds up to one base unit, and so does any share of it;
        // 10,002 * 0.9999 rounds down to 10,000.
        let quote = quote(1, 10_002, "1");
        let params = OrderLimitParams {
            quote: &quote,
            quote_gas_units: 0,
            hook_gas: 1,
            gas_price_wei: 1,
            hook_data_cost_wei: U256::ZERO,
            native_rate: NativeBuyRate::Anchor(U256::ONE),
            price_tolerance_bps: 1,
            gas_share_bps: GAS_SHARE_LOOSE_BPS,
            shield_fee_bps: U256::from(25),
        };
        for share in [GAS_SHARE_TIGHT_BPS, GAS_SHARE_LOOSE_BPS] {
            let limit = price_order_limit(&OrderLimitParams {
                gas_share_bps: share,
                ..params
            })
            .unwrap();
            assert_eq!(
                (limit.gas_estimate, limit.gas_allowance),
                (U256::ONE, U256::ONE)
            );
            assert_eq!(limit.min_received, U256::from(9_975));
            assert_eq!(limit.buy_amount, U256::from(9_999));
        }

        // The network fee converts at the quoted rate and rounds down: 10,002 / 4 is 2,500.5.
        let mut with_fee = self::quote(4, 10_002, "1");
        with_fee.fee_amount = U256::ONE;
        assert_eq!(
            price_order_limit(&OrderLimitParams {
                quote: &with_fee,
                ..params
            })
            .unwrap()
            .best_case,
            U256::from(12_502)
        );

        // Fractional swap gas rounds up; an unusable amount stops pricing.
        for (gas, units) in [
            ("232610", 232_610),
            ("232610.2", 232_611),
            ("2.3261E5", 232_610),
        ] {
            let mut quote = quote.clone();
            quote.gas_amount = gas.into();
            assert_eq!(quote_gas_units(&quote), Ok(units), "{gas}");
        }
        for gas in ["", "NaN", "-1", "1e30"] {
            let mut quote = quote.clone();
            quote.gas_amount = gas.into();
            assert_eq!(
                quote_gas_units(&quote),
                Err(OrderLimitError::InvalidQuoteGas),
                "{gas}"
            );
        }

        // A quote-derived rate of 1e18 / 3 rounds up.
        let quote = self::quote(1, 1, "3");
        assert_eq!(
            quote_native_to_buy_rate(&quote),
            Ok(U256::from(333_333_333_333_333_334_u64))
        );
    }

    #[test]
    fn quote_rate_reads_plain_and_exponent_prices() {
        // 1 USDC buys 0.00025 WETH, and CoW prices a USDC base unit at 2.5e8 wei.
        for price in ["250000000", "250000000.0", "2.5E+8", "2.5e8"] {
            let quote = quote(1_000_000, 250_000_000_000_000, price);
            assert_eq!(
                quote_native_to_buy_rate(&quote),
                Ok(WEI_PER_NATIVE),
                "{price}"
            );
        }
        for price in ["", ".", "-1", "1e", "NaN", "0"] {
            assert_eq!(
                quote_native_to_buy_rate(&quote(1, 1, price)),
                Err(OrderLimitError::InvalidQuotePrice),
                "{price}"
            );
        }
    }

    #[test]
    fn allowed_gas_at_or_above_the_output_is_rejected_with_the_gas_estimate() {
        let mut quote = quote(1, 6_000_000, "1");
        quote.buy_token = Address::repeat_byte(2);
        let params = OrderLimitParams {
            quote: &quote,
            quote_gas_units: 0,
            hook_gas: 2_000_000,
            gas_price_wei: 1_000_000_000,
            hook_data_cost_wei: U256::ZERO,
            native_rate: NativeBuyRate::Anchor(U256::from(3_000_000_000_u64)),
            price_tolerance_bps: 0,
            gas_share_bps: GAS_SHARE_LOOSE_BPS,
            shield_fee_bps: U256::from(25),
        };
        for hook_gas in [2_000_000, 3_000_000] {
            assert_eq!(
                price_order_limit(&OrderLimitParams { hook_gas, ..params }),
                Err(OrderLimitError::HookCostExceedsOutput {
                    buy_token: quote.buy_token,
                    gas_estimate: U256::from(hook_gas * 3),
                    best_case: quote.buy_amount,
                })
            );
        }
        // A smaller share of the same estimate leaves a positive minimum.
        assert!(
            price_order_limit(&OrderLimitParams {
                gas_share_bps: GAS_SHARE_BALANCED_BPS,
                ..params
            })
            .is_ok()
        );
    }
}
