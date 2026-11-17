use alloy::primitives::U512;
use railgun_wallet::tx::{ETHEREUM_GAS_MODEL, POLYGON_GAS_MODEL};

use super::*;

const fn shape(input_count: usize, output_count: usize, has_unshield: bool) -> TransactionShape {
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
    for (sell_amount, price, error) in [
        (1, "", OrderLimitError::QuotePriceNotDecimal),
        (1, ".", OrderLimitError::QuotePriceNotDecimal),
        (1, "-1", OrderLimitError::QuotePriceNotDecimal),
        (1, "1e", OrderLimitError::QuotePriceNotDecimal),
        (1, "NaN", OrderLimitError::QuotePriceNotDecimal),
        (1, "0", OrderLimitError::QuotePriceZero),
        (0, "1", OrderLimitError::QuoteSellAmountZero),
        // 10^(18 + 70) is beyond `U256`.
        (1, "1e-70", OrderLimitError::QuotePriceOutOfRange),
    ] {
        assert_eq!(
            quote_native_to_buy_rate(&quote(sell_amount, 1, price)),
            Err(error),
            "{price}"
        );
    }
}

#[test]
fn quote_rate_cuts_a_long_price_to_eighteen_significant_digits() {
    // `ceil(1e18 * buyAmount / (price * sellAmount))` from every digit of a `0.` price.
    let exact = |quote: &CowQuoteParameters| {
        let fraction = quote.sell_token_price.split_once('.').unwrap().1;
        let numerator =
            U512::from(quote.buy_amount) * U512::from(10_u8).pow(U512::from(18 + fraction.len()));
        let denominator =
            U512::from_str_radix(fraction, 10).unwrap() * U512::from(quote.sell_amount);
        numerator.div_ceil(denominator)
    };
    let rate = |price: &str| {
        let quote = quote(1_000_000_000_000_000_000, 999_000, price);
        let rate = U512::from(quote_native_to_buy_rate(&quote).unwrap());
        (rate, exact(&quote))
    };

    // 17 significant digits are all kept.
    let (short, exact_short) = rate("0.0013443372127831241");
    assert_eq!(short, exact_short);

    // A float's full expansion from a BNB Chain quote, and more digits than `U256` holds.
    let beyond_u256 = format!("0.{}", "123456789".repeat(10));
    for price in [
        "0.0013443372127831241559514641181749539100565016269683837890625",
        beyond_u256.as_str(),
    ] {
        let (cut, exact) = rate(price);
        assert!(exact <= cut && cut <= exact + U512::ONE, "{price}");
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

#[test]
fn delivery_allowance_converts_by_anchor_par_or_the_quotes_gas_fee() {
    // 1,000,000 gas at 2 gwei is 0.002 native, and a rollup's data cost adds 0.001.
    let params = DeliveryAllowanceParams {
        gas: 1_000_000,
        gas_price_wei: 2_000_000_000,
        data_cost_wei: U256::from(1_000_000_000_000_000_u64),
        rate: DeliveryAllowanceRate::Par,
    };
    let wei = U256::from(3_000_000_000_000_000_u64);
    for (rate, allowance) in [
        // 0.003 native at 3,000 USDC per native is 9 USDC.
        (
            DeliveryAllowanceRate::Anchor(U256::from(3_000_000_000_u64)),
            U256::from(9_000_000),
        ),
        // 0.003 * 3,000.000001 is 9,000,000.003 base units, which rounds up.
        (
            DeliveryAllowanceRate::Anchor(U256::from(3_000_000_001_u64)),
            U256::from(9_000_001),
        ),
        (DeliveryAllowanceRate::Par, wei),
        // 8,329 * 1,000,000 / 100,000 is 83,290; neither the price nor the data cost counts.
        (
            DeliveryAllowanceRate::QuoteScaled {
                relayer_gas_fee: U256::from(8_329),
            },
            U256::from(83_290),
        ),
    ] {
        assert_eq!(
            delivery_allowance(&DeliveryAllowanceParams { rate, ..params }),
            Ok(allowance),
            "{rate:?}"
        );
    }
    // 8,329 * 1,000,001 / 100,000 is 83,290.08329, which rounds up.
    assert_eq!(
        delivery_allowance(&DeliveryAllowanceParams {
            gas: 1_000_001,
            rate: DeliveryAllowanceRate::QuoteScaled {
                relayer_gas_fee: U256::from(8_329),
            },
            ..params
        }),
        Ok(U256::from(83_291))
    );
    assert_eq!(
        delivery_allowance(&DeliveryAllowanceParams {
            rate: DeliveryAllowanceRate::Anchor(U256::MAX),
            ..params
        }),
        Err(OrderLimitError::Overflow)
    );

    let output = U256::from(9_000_000);
    assert_eq!(
        destination_minimum_after_allowance(output, U256::from(83_290)),
        Ok(U256::from(8_916_710))
    );
    for allowance in [output, output + U256::ONE] {
        assert_eq!(
            destination_minimum_after_allowance(output, allowance),
            Err(OrderLimitError::DeliveryAllowanceExceedsOutput { allowance, output })
        );
    }
}
