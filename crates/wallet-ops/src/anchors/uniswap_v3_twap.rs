use alloy::primitives::U256;

const MIN_TICK: i32 = -887_272;
const MAX_TICK: i32 = 887_272;

fn mean_tick(tick_cumulatives: &[i128], window_seconds: u32) -> Option<f64> {
    if tick_cumulatives.len() != 2 || window_seconds == 0 {
        return None;
    }
    let delta = tick_cumulatives[1].checked_sub(tick_cumulatives[0])?;
    // The observation delta is intentionally approximated for exp/ln tick math; the result is
    // subsequently checked against the supported mean-tick range.
    #[allow(clippy::cast_precision_loss)]
    let mean = delta as f64 / f64::from(window_seconds);
    if !mean.is_finite() || !(f64::from(MIN_TICK)..=f64::from(MAX_TICK)).contains(&mean) {
        return None;
    }
    Some(mean)
}

fn tick_price_ratio(mean_tick: f64, base_is_token0: bool, base_decimals: u8) -> Option<f64> {
    if !mean_tick.is_finite() || !(f64::from(MIN_TICK)..=f64::from(MAX_TICK)).contains(&mean_tick) {
        return None;
    }
    let mut ratio = (mean_tick * 1.0001_f64.ln()).exp();
    if !base_is_token0 {
        ratio = 1.0 / ratio;
    }
    let scaled = ratio * 10_f64.powi(i32::from(base_decimals));
    (scaled.is_finite() && scaled > 0.0).then_some(scaled)
}

fn f64_to_u256_trunc(value: f64) -> Option<U256> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let bits = value.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    if exponent == 0 {
        return None;
    }
    let mantissa = (1_u64 << 52) | fraction;
    let shift = exponent - 1023 - 52;
    let mut integer = U256::from(mantissa);
    if shift >= 0 {
        integer = integer.checked_shl(usize::try_from(shift).ok()?)?;
    } else {
        let right_shift = usize::try_from(shift.unsigned_abs()).ok()?;
        integer = if right_shift >= 256 {
            U256::ZERO
        } else {
            integer >> right_shift
        };
    }
    (!integer.is_zero()).then_some(integer)
}

pub(super) fn quote_from_observation(
    tick_cumulatives: &[i128],
    window_seconds: u32,
    base_token_is_token0: bool,
    base_token_decimals: u8,
) -> Option<U256> {
    let mean = mean_tick(tick_cumulatives, window_seconds)?;
    f64_to_u256_trunc(tick_price_ratio(
        mean,
        base_token_is_token0,
        base_token_decimals,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_tick_validates_window_shape_and_bounds() {
        assert_eq!(mean_tick(&[0, 1_800], 1_800), Some(1.0));
        assert_eq!(mean_tick(&[1_800, 0], 1_800), Some(-1.0));
        assert_eq!(mean_tick(&[0, 1], 0), None);
        assert_eq!(mean_tick(&[0], 1), None);
        assert_eq!(mean_tick(&[0, i128::from(MAX_TICK) + 1], 1), None);
        assert_eq!(mean_tick(&[i128::MAX, i128::MIN], 1), None);
        assert_eq!(
            mean_tick(&[0, i128::from(MIN_TICK)], 1),
            Some(f64::from(MIN_TICK))
        );
        assert_eq!(
            mean_tick(&[0, i128::from(MAX_TICK)], 1),
            Some(f64::from(MAX_TICK))
        );
    }

    #[test]
    fn tick_price_reference_vectors_stay_within_one_basis_point() {
        for tick in [-887_272.0, -100.0, 0.0, 100.0, 887_272.0] {
            let expected = (1.0001_f64).powf(tick);
            let actual = tick_price_ratio(tick, true, 0).expect("token0 ratio");
            assert!(((actual - expected).abs() / expected) <= 0.0001);
            let inverse = tick_price_ratio(tick, false, 18).expect("token1 ratio");
            let expected_inverse = (1.0 / expected) * 1e18;
            assert!(((inverse - expected_inverse).abs() / expected_inverse) <= 0.0001);
        }
    }

    #[test]
    fn fractional_mean_tick_matches_independent_orientation_references() {
        let positive_mean = mean_tick(&[0, 1], 2).expect("fractional mean tick");
        assert!((positive_mean - 0.5).abs() <= f64::EPSILON);
        let token0_quote = tick_price_ratio(positive_mean, true, 6).expect("token0 quote");
        assert!(
            ((token0_quote - 1_000_049.998_750_062_5).abs() / 1_000_049.998_750_062_5) <= 0.0001
        );

        let negative_mean = mean_tick(&[0, -1], 2).expect("signed fractional mean tick");
        assert!((negative_mean + 0.5).abs() <= f64::EPSILON);
        let token1_quote = tick_price_ratio(negative_mean, false, 18).expect("token1 quote");
        assert!(
            ((token1_quote - 1_000_049_998_750_062_500.0).abs() / 1_000_049_998_750_062_500.0)
                <= 0.0001
        );
    }

    #[test]
    fn f64_conversion_handles_fractional_large_and_invalid_values() {
        assert_eq!(f64_to_u256_trunc(0.5), None);
        assert_eq!(f64_to_u256_trunc(0.999_999), None);
        assert_eq!(f64_to_u256_trunc(1.9), Some(U256::from(1)));
        assert_eq!(f64_to_u256_trunc(42.9), Some(U256::from(42)));
        assert!(f64_to_u256_trunc(2_f64.powi(200)).is_some());
        assert!(f64_to_u256_trunc(2_f64.powi(255)).is_some());
        assert_eq!(f64_to_u256_trunc(2_f64.powi(256)), None);
        assert_eq!(f64_to_u256_trunc(-1.0), None);
        assert_eq!(f64_to_u256_trunc(0.0), None);
        assert_eq!(f64_to_u256_trunc(f64::NEG_INFINITY), None);
        assert_eq!(f64_to_u256_trunc(f64::INFINITY), None);
        assert_eq!(f64_to_u256_trunc(f64::NAN), None);
    }

    #[test]
    fn quote_applies_orientation_and_decimal_scaling() {
        let token0_quote = quote_from_observation(&[0, 0], 1_800, true, 6).expect("quote");
        assert_eq!(token0_quote, U256::from(1_000_000));
        let token1_quote = quote_from_observation(&[0, 0], 1_800, false, 18).expect("quote");
        assert_eq!(token1_quote, U256::from(1_000_000_000_000_000_000_u128));
    }
}
