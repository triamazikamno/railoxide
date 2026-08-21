//! Static registry of the tokens the broadcaster accepts fees in, plus
//! the display helpers the fees pane uses to render them.
//!
//! The table is mirrored verbatim from `config.example.yaml` `chains[].fees`
//! — `!Oracle` entries copy `token_decimals` exactly, `!Fixed` entries
//! default to 18 (all wrapped-native tokens). When operators run with a
//! different config we fall through to the raw-address / raw-integer
//! display, which is the signal to extend this list.

use std::path::PathBuf;
use std::sync::LazyLock;

use alloy::primitives::{Address, address};
use ruint::aliases::U256;
use ruint::uint;

static TOKEN_ICON_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/tokens"));

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
    pub symbol: &'static str,
    pub decimals: u8,
    pub anchor_sources: &'static [TokenAnchorSource],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TokenAnchorInfo {
    pub chain_id: u64,
    pub token: Address,
    pub anchor_sources: &'static [TokenAnchorSource],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NativeUsdAnchorInfo {
    pub chain_id: u64,
    pub anchor_sources: &'static [TokenAnchorSource],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KnownTokenInfo {
    pub chain_id: u64,
    pub token: Address,
    pub symbol: &'static str,
    pub decimals: u8,
    pub anchor_sources: &'static [TokenAnchorSource],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TokenAnchorSource {
    Fixed {
        token_fee_per_unit_gas: U256,
    },
    ChainlinkOracle {
        addr: Address,
        token_decimals: u8,
        oracle_decimals: u8,
        is_inversed: bool,
    },
    UniswapV3Twap {
        pool: Address,
        base_token: Address,
        quote_token: Address,
        base_token_decimals: u8,
        window_seconds: u32,
    },
    Product {
        sources: &'static [Self],
        scale_decimals: u8,
    },
}

pub const WRAPPED_NATIVE_FEE_RATE: U256 = uint!(1_000_000_000_000_000_000_U256);
const USD_MICRO_PER_CENT: U256 = uint!(10_000_U256);
const USD_MICRO_PER_DOLLAR: U256 = uint!(1_000_000_U256);
const CENTS_PER_DOLLAR: U256 = uint!(100_U256);
const USD_REDUNDANCY_BASIS_POINTS: U256 = uint!(10_000_U256);
const USD_REDUNDANCY_TOLERANCE_BASIS_POINTS: U256 = uint!(200_U256);

const NO_ANCHORS: &[TokenAnchorSource] = &[];
const WRAPPED_NATIVE_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::Fixed {
    token_fee_per_unit_gas: WRAPPED_NATIVE_FEE_RATE,
}];
const ETH_USD_6_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"),
    token_decimals: 6,
    oracle_decimals: 8,
    is_inversed: false,
}];
const ETH_USD_18_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"),
    token_decimals: 18,
    oracle_decimals: 8,
    is_inversed: false,
}];
const BTC_ETH_8_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0xdeb288F737066589598e9214E782fa5A8eD689e8"),
    token_decimals: 8,
    oracle_decimals: 18,
    is_inversed: true,
}];
const BNB_USD_18_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE"),
    token_decimals: 18,
    oracle_decimals: 8,
    is_inversed: false,
}];
const MATIC_USD_6_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0xAB594600376Ec9fD91F8e885dADF0CE036862dE0"),
    token_decimals: 6,
    oracle_decimals: 8,
    is_inversed: false,
}];
const MATIC_USD_18_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0xAB594600376Ec9fD91F8e885dADF0CE036862dE0"),
    token_decimals: 18,
    oracle_decimals: 8,
    is_inversed: false,
}];
const ARB_ETH_USD_6_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612"),
    token_decimals: 6,
    oracle_decimals: 8,
    is_inversed: false,
}];
const ARB_ETH_USD_18_SOURCE: TokenAnchorSource = TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612"),
    token_decimals: 18,
    oracle_decimals: 8,
    is_inversed: false,
};
const ARB_ETH_USD_18_ANCHOR: &[TokenAnchorSource] = &[ARB_ETH_USD_18_SOURCE];
const ARB_BTC_ETH_8_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0xc5a90A6d7e4Af242dA238FFe279e9f2BA0c64B2e"),
    token_decimals: 8,
    oracle_decimals: 18,
    is_inversed: true,
}];
const ARB_USD_INVERSE_18_SOURCE: TokenAnchorSource = TokenAnchorSource::ChainlinkOracle {
    addr: address!("0xB72359B2dc04Ff363e094648DF78247c98297c20"),
    token_decimals: 18,
    oracle_decimals: 8,
    is_inversed: true,
};
const ARB_PER_ETH_18_PRODUCT_SOURCES: &[TokenAnchorSource] =
    &[ARB_ETH_USD_18_SOURCE, ARB_USD_INVERSE_18_SOURCE];
const ARB_PER_ETH_18_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::Product {
    sources: ARB_PER_ETH_18_PRODUCT_SOURCES,
    scale_decimals: 18,
}];
const BNB_USD_6_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE"),
    token_decimals: 6,
    oracle_decimals: 8,
    is_inversed: false,
}];
const MATIC_USD_6_NATIVE_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0xAB594600376Ec9fD91F8e885dADF0CE036862dE0"),
    token_decimals: 6,
    oracle_decimals: 8,
    is_inversed: false,
}];
const ARB_ETH_USD_6_NATIVE_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612"),
    token_decimals: 6,
    oracle_decimals: 8,
    is_inversed: false,
}];
const RAIL_ETH_TWAP_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::UniswapV3Twap {
    pool: address!("0x2837809FD68e4a4104af76bbec5b622b6146B2cb"),
    base_token: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
    quote_token: address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D"),
    base_token_decimals: 18,
    window_seconds: 1_800,
}];

#[rustfmt::skip]
const NATIVE_USD_ANCHORS: &[(u64, &[TokenAnchorSource])] = &[
    (1, ETH_USD_6_ANCHOR),
    (56, BNB_USD_6_ANCHOR),
    (137, MATIC_USD_6_NATIVE_ANCHOR),
    (42161, ARB_ETH_USD_6_NATIVE_ANCHOR),
];

#[rustfmt::skip]
const TOKENS: &[(u64, Address, &str, u8, &[TokenAnchorSource])] = &[
    // Ethereum (1)
    (1, address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), "WETH", 18, WRAPPED_NATIVE_ANCHOR),
    (1, address!("0xdAC17F958D2ee523a2206206994597C13D831ec7"), "USDT", 6, ETH_USD_6_ANCHOR),
    (1, address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), "USDC", 6, ETH_USD_6_ANCHOR),
    (1, address!("0x6b175474e89094c44da98b954eedeac495271d0f"), "DAI", 18, ETH_USD_18_ANCHOR),
    (1, address!("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"), "WBTC", 8, BTC_ETH_8_ANCHOR),
    (1, address!("0x1aBaEA1f7C830bD89Acc67eC4af516284b1bC33c"), "EURC", 6, NO_ANCHORS),
    (1, address!("0x6f40d4a6237c257fff2db00fa0510deeecd303eb"), "FLUID", 18, NO_ANCHORS),
    (1, address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D"), "RAIL", 18, RAIL_ETH_TWAP_ANCHOR),
    (1, address!("0x03ab458634910aad20ef5f1c8ee96f1d6ac54919"), "RAI", 18, NO_ANCHORS),
    (1, address!("0x853d955aCEf822Db058eb8505911ED77F175b99e"), "FRAX", 18, ETH_USD_18_ANCHOR),
    (1, address!("0x956f47f50a910163d8bf957cf5846d573e7f87ca"), "FEI", 18, ETH_USD_18_ANCHOR),
    (1, address!("0xeb4c2781e4eba804ce9a9803c67d0893436bb27d"), "renBTC", 8, BTC_ETH_8_ANCHOR),
    (1, address!("0x085780639CC2cACd35E474e71f4d000e2405d8f6"), "fxUSD", 18, ETH_USD_18_ANCHOR),
    (1, address!("0x4c9EDD5852cd905f086C759E8383e09bff1E68B3"), "USDe", 18, ETH_USD_18_ANCHOR),
    (1, address!("0x4f8e5DE400DE08B164E7421B3EE387f461beCD1A"), "USDD", 18, ETH_USD_18_ANCHOR),
    (1, address!("0x8d0D000Ee44948FC98c9B98A4FA4921476f08B0d"), "USD1", 18, ETH_USD_18_ANCHOR),
    (1, address!("0xdC035D45d973E3EC169d2276DDab16f1e407384F"), "USDS", 18, ETH_USD_18_ANCHOR),
    (1, address!("0xe343167631d89B6Ffc58B88d6b7fB0228795491D"), "USDG", 6, ETH_USD_6_ANCHOR),
    (1, address!("0xFa2B947eEc368f42195f24F36d2aF29f7c24CeC2"), "USDF", 18, ETH_USD_18_ANCHOR),
    (1, address!("0x514910771AF9Ca656af840dff83E8264EcF986CA"), "LINK", 18, NO_ANCHORS),
    (1, address!("0x6c3ea9036406852006290770BEdFcAbA0e23A0e8"), "PYUSD", 6, ETH_USD_6_ANCHOR),
    // BSC (56)
    (56, address!("0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c"), "WBNB", 18, WRAPPED_NATIVE_ANCHOR),
    (56, address!("0x55d398326f99059ff775485246999027b3197955"), "BSC-USD", 18, BNB_USD_18_ANCHOR),
    (56, address!("0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d"), "USDC", 18, BNB_USD_18_ANCHOR),
    (56, address!("0xe9e7cea3dedca5984780bafc599bd69add087d56"), "BUSD", 18, BNB_USD_18_ANCHOR),
    (56, address!("0x1af3f329e8be154074d8769d1ffa4ee058b1dbc3"), "DAI", 18, BNB_USD_18_ANCHOR),
    (56, address!("0x0E09FaBB73Bd3Ade0a17ECC321fD13a19e81cE82"), "CAKE", 18, NO_ANCHORS),
    (56, address!("0x2170Ed0880ac9A755fd29B2688956BD959F933F8"), "ETH", 18, NO_ANCHORS),
    // Polygon (137)
    (137, address!("0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270"), "WMATIC", 18, WRAPPED_NATIVE_ANCHOR),
    (137, address!("0xc2132d05d31c914a87c6611c10748aeb04b58e8f"), "USDT", 6, MATIC_USD_6_ANCHOR),
    (137, address!("0x2791bca1f2de4661ed88a30c99a7a9449aa84174"), "USDC.e", 6, MATIC_USD_6_ANCHOR),
    (137, address!("0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359"), "USDC", 6, MATIC_USD_6_ANCHOR),
    (137, address!("0x8f3cf7ad23cd3cadbd9735aff958023239c6a063"), "DAI", 18, MATIC_USD_18_ANCHOR),
    (137, address!("0x1BFD67037B42Cf73acF2047067bd4F2C47D9BfD6"), "WBTC", 8, NO_ANCHORS),
    (137, address!("0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619"), "WETH", 18, NO_ANCHORS),
    // Arbitrum (42161)
    (42161, address!("0x82af49447d8a07e3bd95bd0d56f35241523fbab1"), "WETH", 18, WRAPPED_NATIVE_ANCHOR),
    (42161, address!("0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9"), "USDT", 6, ARB_ETH_USD_6_ANCHOR),
    (42161, address!("0xff970a61a04b1ca14834a43f5de4533ebddb5cc8"), "USDC.e", 6, ARB_ETH_USD_6_ANCHOR),
    (42161, address!("0xaf88d065e77c8cc2239327c5edb3a432268e5831"), "USDC", 6, ARB_ETH_USD_6_ANCHOR),
    (42161, address!("0xda10009cbd5d07dd0cecc66161fc93d7c9000da1"), "DAI", 18, ARB_ETH_USD_18_ANCHOR),
    (42161, address!("0x2f2a2543b76a4166549f7aab2e75bef0aefc5b0f"), "WBTC", 8, ARB_BTC_ETH_8_ANCHOR),
    (42161, address!("0x912ce59144191c1204e64559fe8253a0e49e6548"), "ARB", 18, ARB_PER_ETH_18_ANCHOR),
    (42161, address!("0xFa7F8980b0f1E64A2062791cc3b0871572f1F7f0"), "UNI", 18, NO_ANCHORS),
    (42161, address!("0x17FC002b466eEc40DaE837Fc4bE5c67993ddBd6F"), "FRAX", 18, ARB_ETH_USD_18_ANCHOR),
    (42161, address!("0x4D15a3A2286D883AF0AA1B3f21367843FAc63E07"), "TUSD", 18, ARB_ETH_USD_18_ANCHOR),
];

#[must_use]
pub fn lookup_token(chain_id: u64, addr: &Address) -> Option<TokenInfo> {
    TOKENS
        .iter()
        .find(|(c, a, _, _, _)| *c == chain_id && a == addr)
        .map(|(_, _, symbol, decimals, anchor_sources)| TokenInfo {
            symbol,
            decimals: *decimals,
            anchor_sources,
        })
}

pub fn token_anchor_entries() -> impl Iterator<Item = TokenAnchorInfo> {
    TOKENS
        .iter()
        .filter(|(_, _, _, _, anchor_sources)| !anchor_sources.is_empty())
        .map(|(chain_id, token, _, _, anchor_sources)| TokenAnchorInfo {
            chain_id: *chain_id,
            token: *token,
            anchor_sources,
        })
}

pub fn native_usd_anchor_entries() -> impl Iterator<Item = NativeUsdAnchorInfo> {
    NATIVE_USD_ANCHORS
        .iter()
        .map(|(chain_id, anchor_sources)| NativeUsdAnchorInfo {
            chain_id: *chain_id,
            anchor_sources,
        })
}

pub fn known_tokens_for_chain(chain_id: u64) -> impl Iterator<Item = KnownTokenInfo> {
    TOKENS
        .iter()
        .filter(move |(token_chain_id, _, _, _, _)| *token_chain_id == chain_id)
        .map(
            |(chain_id, token, symbol, decimals, anchor_sources)| KnownTokenInfo {
                chain_id: *chain_id,
                token: *token,
                symbol,
                decimals: *decimals,
                anchor_sources,
            },
        )
}

#[must_use]
pub fn token_icon_path(chain_id: u64, addr: &Address) -> Option<PathBuf> {
    token_icon_file_name(chain_id, addr).map(|file| TOKEN_ICON_DIR.join(file))
}

#[must_use]
pub fn token_icon_asset_path(chain_id: u64, addr: &Address) -> Option<String> {
    token_icon_file_name(chain_id, addr).map(|file| format!("railgun-ui/tokens/{file}"))
}

fn token_icon_file_name(chain_id: u64, addr: &Address) -> Option<String> {
    lookup_token(chain_id, addr)?;
    let ext = if (chain_id == 1 && *addr == address!("0x085780639CC2cACd35E474e71f4d000e2405d8f6"))
        || (chain_id == 42161 && *addr == address!("0x4D15a3A2286D883AF0AA1B3f21367843FAc63E07"))
    {
        "svg"
    } else {
        "png"
    };

    Some(format!("{chain_id}-{addr:#x}.{ext}"))
}

fn pow10(exp: u8) -> U256 {
    uint!(10_U256).pow(U256::from(exp))
}

#[must_use]
pub fn format_scaled_amount(amount: U256, decimals: u8) -> String {
    if decimals == 0 {
        return amount.to_string();
    }
    let divisor = pow10(decimals);
    let whole = amount / divisor;
    let frac = amount % divisor;
    let frac_str = frac.to_string();
    let padded = format!("{frac_str:0>width$}", width = decimals as usize);
    let trimmed = padded.trim_end_matches('0');
    if trimmed.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{trimmed}")
    }
}

fn display_precision(amount: U256, decimals: u8) -> u8 {
    if decimals == 0 {
        return 0;
    }

    let scale = pow10(decimals);
    let precision = if amount >= scale * uint!(100_U256) {
        0
    } else if amount >= scale {
        2
    } else {
        let tenth = pow10(decimals - 1);
        if amount >= uint!(5_U256) * tenth {
            4
        } else if amount >= tenth {
            5
        } else {
            6
        }
    };

    precision.min(decimals)
}

fn format_token_amount_with_precision(amount: U256, decimals: u8, precision: u8) -> String {
    debug_assert!(precision <= decimals);

    if precision == decimals {
        return format_scaled_amount(amount, decimals);
    }

    let rounding_divisor = pow10(decimals - precision);
    let mut rounded = amount / rounding_divisor;
    let remainder = amount % rounding_divisor;
    if remainder >= rounding_divisor / uint!(2_U256) {
        rounded += uint!(1_U256);
    }

    format_scaled_amount(rounded, precision)
}

/// Format a raw integer amount as a decimal string scaled by `decimals`,
/// using coarse precision for large values and finer precision for small
/// values so fee cells stay readable.
#[must_use]
pub fn format_token_amount(amount: U256, decimals: u8) -> String {
    format_token_amount_with_precision(amount, decimals, display_precision(amount, decimals))
}

/// Format an upper-bound token amount without rendering below the raw value.
#[must_use]
pub fn format_token_amount_ceiling(amount: U256, decimals: u8) -> String {
    let precision = display_precision(amount, decimals);
    if precision == decimals {
        return format_scaled_amount(amount, decimals);
    }
    let rounding_divisor = pow10(decimals - precision);
    let mut rounded = amount / rounding_divisor;
    if amount % rounding_divisor != U256::ZERO {
        rounded += uint!(1_U256);
    }
    format_scaled_amount(rounded, precision)
}

#[must_use]
pub fn token_usd_micro_value(
    amount: U256,
    token_anchor_rate: U256,
    native_usd_micro_rate: U256,
) -> Option<U256> {
    if token_anchor_rate.is_zero() || native_usd_micro_rate.is_zero() {
        return None;
    }
    amount
        .checked_mul(native_usd_micro_rate)?
        .checked_div(token_anchor_rate)
}

#[must_use]
pub fn native_usd_micro_value(amount: U256, native_usd_micro_rate: U256) -> Option<U256> {
    token_usd_micro_value(amount, WRAPPED_NATIVE_FEE_RATE, native_usd_micro_rate)
}

/// Returns no supplemental USD value when the token's valuation is within 2% of $1 per token.
#[must_use]
pub fn non_redundant_usd_micro_value(
    token_amount: U256,
    token_decimals: u8,
    usd_micro_value: U256,
) -> Option<U256> {
    let Some(token_scale) = U256::from(10).checked_pow(U256::from(token_decimals)) else {
        return Some(usd_micro_value);
    };
    let Some(scaled_usd_value) = usd_micro_value
        .checked_mul(token_scale)
        .and_then(|value| value.checked_mul(USD_REDUNDANCY_BASIS_POINTS))
    else {
        return Some(usd_micro_value);
    };
    let Some(nominal_usd_value) = token_amount.checked_mul(USD_MICRO_PER_DOLLAR) else {
        return Some(usd_micro_value);
    };
    let Some(lower_bound) = nominal_usd_value
        .checked_mul(USD_REDUNDANCY_BASIS_POINTS - USD_REDUNDANCY_TOLERANCE_BASIS_POINTS)
    else {
        return Some(usd_micro_value);
    };
    let Some(upper_bound) = nominal_usd_value
        .checked_mul(USD_REDUNDANCY_BASIS_POINTS + USD_REDUNDANCY_TOLERANCE_BASIS_POINTS)
    else {
        return Some(usd_micro_value);
    };

    (!(lower_bound..=upper_bound).contains(&scaled_usd_value)).then_some(usd_micro_value)
}

#[must_use]
pub fn format_usd_micro_value(value: U256) -> String {
    let mut rounded_cents = value / USD_MICRO_PER_CENT;
    if value % USD_MICRO_PER_CENT >= USD_MICRO_PER_CENT / uint!(2_U256) {
        rounded_cents = rounded_cents.saturating_add(U256::ONE);
    }
    let dollars = format_usd_dollars(rounded_cents / CENTS_PER_DOLLAR);
    let cents = (rounded_cents % CENTS_PER_DOLLAR).to_string();
    format!("${dollars}.{cents:0>2}")
}

fn format_usd_dollars(dollars: U256) -> String {
    let digits = dollars.to_string();
    if digits.len() <= 3 {
        return digits;
    }
    let mut formatted = String::with_capacity(digits.len() + (digits.len() - 1) / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

/// Shorten an address for the fallback display on unknown tokens.
/// Produces `"0xc02a…6cc2"` — 4 hex chars on each side, enough to
/// distinguish tokens without burning a full 42-char column.
#[must_use]
pub fn short_address(addr: &Address) -> String {
    let hex = format!("{addr:#x}");
    format!("{}…{}", &hex[..6], &hex[38..])
}

/// Format a broadcaster Railgun address the same way across wallet and monitor
/// surfaces. 0zk addresses are ASCII base32, so slicing the
/// final 4 bytes is safe for current address strings.
#[must_use]
pub fn format_broadcaster_address_label(address: &str, identifier: Option<&str>) -> String {
    let last4 = &address[address.len().saturating_sub(4)..];
    match identifier {
        Some(identifier) if !identifier.is_empty() => format!("0zk...{last4} ({identifier})"),
        _ => format!("0zk...{last4}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_handles_zero_decimals() {
        assert_eq!(format_token_amount(uint!(123_U256), 0), "123");
    }

    #[test]
    fn format_inclusive_thresholds_pick_expected_precision() {
        assert_eq!(display_precision(uint!(100_000_000_U256), 6), 0);
        assert_eq!(display_precision(uint!(1_000_000_U256), 6), 2);
        assert_eq!(display_precision(uint!(500_000_U256), 6), 4);
        assert_eq!(display_precision(uint!(100_000_U256), 6), 5);
        assert_eq!(display_precision(uint!(99_999_U256), 6), 6);
        assert_eq!(display_precision(uint!(99_999_999_U256), 6), 2);
    }

    #[test]
    fn format_trims_trailing_zeros_after_rounding() {
        assert_eq!(format_token_amount(uint!(1_000_000_U256), 6), "1");
        assert_eq!(format_token_amount(uint!(1_500_000_U256), 6), "1.5");
    }

    #[test]
    fn format_rounds_large_values_to_whole_numbers() {
        assert_eq!(
            format_token_amount(uint!(19_232_527_572_893_U256), 9),
            "19233"
        );
    }

    #[test]
    fn format_uses_two_decimals_between_one_and_hundred() {
        assert_eq!(format_token_amount(uint!(12_345_600_U256), 6), "12.35");
    }

    #[test]
    fn format_uses_four_decimals_between_half_and_one() {
        assert_eq!(format_token_amount(uint!(543_250_U256), 6), "0.5433");
    }

    #[test]
    fn format_uses_five_decimals_between_tenth_and_half() {
        assert_eq!(format_token_amount(uint!(123_456_789_U256), 9), "0.12346");
    }

    #[test]
    fn format_uses_six_decimals_below_tenth() {
        assert_eq!(format_token_amount(uint!(12_345_U256), 6), "0.012345");
    }

    #[test]
    fn precision_caps_to_available_token_decimals() {
        assert_eq!(display_precision(uint!(54_U256), 2), 2);
        assert_eq!(format_token_amount(uint!(54_U256), 2), "0.54");
    }

    #[test]
    fn format_zero_amount() {
        assert_eq!(format_token_amount(U256::ZERO, 18), "0");
        assert_eq!(format_token_amount(U256::ZERO, 0), "0");
    }

    #[test]
    fn format_upper_bound_never_rounds_below_raw_value() {
        assert_eq!(format_token_amount_ceiling(uint!(1_004_U256), 1), "101");
        assert_eq!(format_token_amount_ceiling(uint!(12_341_U256), 3), "12.35");
        assert_eq!(format_token_amount_ceiling(uint!(12_340_U256), 3), "12.34");
        assert_eq!(format_token_amount_ceiling(uint!(123_U256), 0), "123");
    }

    #[test]
    fn lookup_hits_ethereum_weth() {
        let addr = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let info = lookup_token(1, &addr).expect("WETH on Ethereum should be known");
        assert_eq!(info.symbol, "WETH");
        assert_eq!(info.decimals, 18);
        assert_eq!(
            info.anchor_sources,
            &[TokenAnchorSource::Fixed {
                token_fee_per_unit_gas: WRAPPED_NATIVE_FEE_RATE,
            }]
        );
        assert!(token_icon_path(1, &addr).is_some_and(|path| {
            path.ends_with("assets/tokens/1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")
        }));
    }

    #[test]
    fn lookup_exposes_oracle_anchor_sources() {
        let addr = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let info = lookup_token(1, &addr).expect("USDC on Ethereum should be known");

        assert_eq!(info.symbol, "USDC");
        assert_eq!(info.decimals, 6);
        assert_eq!(
            info.anchor_sources,
            &[TokenAnchorSource::ChainlinkOracle {
                addr: address!("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"),
                token_decimals: 6,
                oracle_decimals: 8,
                is_inversed: false,
            }]
        );
    }

    #[test]
    fn native_usd_anchor_entries_cover_supported_wallet_chains() {
        let entries = native_usd_anchor_entries().collect::<Vec<_>>();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.chain_id)
                .collect::<Vec<_>>(),
            [1, 56, 137, 42161]
        );
        assert!(entries.iter().all(|entry| !entry.anchor_sources.is_empty()));
        assert_eq!(
            entries[0].anchor_sources,
            &[TokenAnchorSource::ChainlinkOracle {
                addr: address!("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"),
                token_decimals: 6,
                oracle_decimals: 8,
                is_inversed: false,
            }]
        );
    }

    #[test]
    fn usd_value_helpers_price_tokens_and_native_assets() {
        let native_usd = uint!(3_000_000_000_U256);

        assert_eq!(
            token_usd_micro_value(uint!(1_500_000_U256), uint!(3_000_000_000_U256), native_usd),
            Some(uint!(1_500_000_U256))
        );
        assert_eq!(
            token_usd_micro_value(
                uint!(500_000_000_000_000_000_U256),
                WRAPPED_NATIVE_FEE_RATE,
                native_usd,
            ),
            Some(uint!(1_500_000_000_U256))
        );
        assert_eq!(
            native_usd_micro_value(uint!(2_000_000_000_000_000_000_U256), native_usd),
            Some(uint!(6_000_000_000_U256))
        );
    }

    #[test]
    fn usd_value_helpers_skip_missing_or_invalid_rates() {
        assert_eq!(
            token_usd_micro_value(uint!(1_U256), U256::ZERO, uint!(1_U256)),
            None
        );
        assert_eq!(
            token_usd_micro_value(uint!(1_U256), uint!(1_U256), U256::ZERO),
            None
        );
        assert_eq!(
            token_usd_micro_value(U256::MAX, uint!(1_U256), uint!(2_U256)),
            None
        );
    }

    #[test]
    fn redundant_usd_value_filter_uses_inclusive_two_percent_bounds() {
        let amount = uint!(100_000_000_U256);

        assert_eq!(
            non_redundant_usd_micro_value(amount, 6, uint!(98_000_000_U256)),
            None
        );
        assert_eq!(
            non_redundant_usd_micro_value(amount, 6, uint!(102_000_000_U256)),
            None
        );
        assert_eq!(
            non_redundant_usd_micro_value(amount, 6, uint!(97_999_999_U256)),
            Some(uint!(97_999_999_U256))
        );
        assert_eq!(
            non_redundant_usd_micro_value(amount, 6, uint!(102_000_001_U256)),
            Some(uint!(102_000_001_U256))
        );
    }

    #[test]
    fn redundant_usd_value_filter_handles_decimals_dust_and_overflow() {
        assert_eq!(
            non_redundant_usd_micro_value(
                uint!(2_000_000_000_000_000_000_U256),
                18,
                uint!(2_000_000_U256),
            ),
            None
        );
        assert_eq!(
            non_redundant_usd_micro_value(uint!(1_U256), 18, U256::ZERO),
            Some(U256::ZERO)
        );
        assert_eq!(
            non_redundant_usd_micro_value(U256::MAX, 18, U256::MAX),
            Some(U256::MAX)
        );
    }

    #[test]
    fn usd_value_formatter_rounds_to_cents() {
        assert_eq!(format_usd_micro_value(U256::ZERO), "$0.00");
        assert_eq!(format_usd_micro_value(uint!(12_344_U256)), "$0.01");
        assert_eq!(format_usd_micro_value(uint!(12_345_U256)), "$0.01");
        assert_eq!(format_usd_micro_value(uint!(123_454_999_U256)), "$123.45");
        assert_eq!(format_usd_micro_value(uint!(123_455_000_U256)), "$123.46");
        assert_eq!(
            format_usd_micro_value(uint!(1_234_560_000_U256)),
            "$1,234.56"
        );
        assert_eq!(
            format_usd_micro_value(uint!(12_345_678_900_000_U256)),
            "$12,345,678.90"
        );
    }

    #[test]
    fn lookup_rail_exposes_builtin_uniswap_v3_twap_source() {
        let addr = address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D");
        let info = lookup_token(1, &addr).expect("RAIL on Ethereum should be known");

        assert_eq!(info.symbol, "RAIL");
        assert_eq!(info.decimals, 18);
        assert_eq!(
            info.anchor_sources,
            &[TokenAnchorSource::UniswapV3Twap {
                pool: address!("0x2837809FD68e4a4104af76bbec5b622b6146B2cb"),
                base_token: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                quote_token: addr,
                base_token_decimals: 18,
                window_seconds: 1_800,
            }]
        );
    }

    #[test]
    fn lookup_exposes_composite_anchor_sources() {
        let addr = address!("0x912ce59144191c1204e64559fe8253a0e49e6548");
        let info = lookup_token(42161, &addr).expect("ARB on Arbitrum should be known");

        assert_eq!(info.symbol, "ARB");
        assert_eq!(info.decimals, 18);
        let [
            TokenAnchorSource::Product {
                sources,
                scale_decimals,
            },
        ] = info.anchor_sources
        else {
            panic!("ARB should use a composite anchor source");
        };
        assert_eq!(*scale_decimals, 18);
        assert_eq!(sources.len(), 2);
    }

    #[test]
    fn lookup_disambiguates_native_usdc_across_chains() {
        // Native Arbitrum USDC uses 6 token decimals in the example config.
        let arb_usdc = address!("0xaf88d065e77c8cc2239327c5edb3a432268e5831");
        let info = lookup_token(42161, &arb_usdc).expect("Arbitrum USDC present");
        assert_eq!(info.symbol, "USDC");
        assert_eq!(info.decimals, 6);

        // Same chain_id with a different address should miss.
        let bogus = address!("0x0000000000000000000000000000000000000001");
        assert!(lookup_token(42161, &bogus).is_none());
    }

    #[test]
    fn lookup_misses_unknown_chain() {
        let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        // Optimism (10) isn't in the registry.
        assert!(lookup_token(10, &weth).is_none());
        assert_eq!(token_icon_path(10, &weth), None);
    }

    #[test]
    fn token_icon_path_uses_svg_for_vendored_svg_icons() {
        let fxusd = address!("0x085780639CC2cACd35E474e71f4d000e2405d8f6");
        assert!(token_icon_path(1, &fxusd).is_some_and(|path| {
            path.ends_with("assets/tokens/1-0x085780639cc2cacd35e474e71f4d000e2405d8f6.svg")
        }));

        let tusd = address!("0x4D15a3A2286D883AF0AA1B3f21367843FAc63E07");
        assert!(token_icon_path(42161, &tusd).is_some_and(|path| {
            path.ends_with("assets/tokens/42161-0x4d15a3a2286d883af0aa1b3f21367843fac63e07.svg")
        }));
    }

    #[test]
    fn short_address_preserves_prefix_and_suffix() {
        let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        assert_eq!(short_address(&weth), "0xc02a…6cc2");
    }

    #[test]
    fn broadcaster_address_label_matches_monitor_style() {
        let address = "0zk1abcdefghijklmnopqrstuvwxyz";
        assert_eq!(
            format_broadcaster_address_label(address, None),
            "0zk...wxyz"
        );
        assert_eq!(
            format_broadcaster_address_label(address, Some("node")),
            "0zk...wxyz (node)"
        );
    }

    #[test]
    fn chain_name_covers_default_set_and_misses_others() {
        use crate::chains::chain_name;

        assert_eq!(chain_name(1), Some("Ethereum"));
        assert_eq!(chain_name(56), Some("BSC"));
        assert_eq!(chain_name(137), Some("Polygon"));
        assert_eq!(chain_name(42161), Some("Arbitrum"));
        assert_eq!(chain_name(10), None);
        assert_eq!(chain_name(0), None);
    }

    #[test]
    fn chain_icon_path_covers_default_set_and_misses_others() {
        use crate::chains::chain_icon_path;

        assert!(
            chain_icon_path(1).is_some_and(|path| path.ends_with("assets/chains/ethereum.svg"))
        );
        assert!(chain_icon_path(56).is_some_and(|path| path.ends_with("assets/chains/bsc.svg")));
        assert!(
            chain_icon_path(137).is_some_and(|path| path.ends_with("assets/chains/polygon.svg"))
        );
        assert!(
            chain_icon_path(42161).is_some_and(|path| path.ends_with("assets/chains/arbitrum.svg"))
        );
        assert_eq!(chain_icon_path(10), None);
    }
}
