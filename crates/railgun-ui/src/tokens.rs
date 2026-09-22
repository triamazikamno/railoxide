//! Static registry of the tokens the broadcaster accepts fees in, plus
//! the display helpers the fees pane uses to render them.
//!
//! The table is mirrored verbatim from `config.example.yaml` `chains[].fees`
//! — `!Oracle` entries copy `token_decimals` exactly, `!Fixed` entries
//! default to 18 (all wrapped-native tokens). When operators run with a
//! different config we fall through to the raw-address / raw-integer
//! display, which is the signal to extend this list.
//!
//! The rows beyond the `config.example.yaml` set cover the other built-in
//! chains; their anchors come from the Chainlink feed proxies listed below,
//! and they reuse vendored icon files rather than shipping new ones.

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

/// Which icon file a known token renders with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TokenIcon {
    /// Derive `{chain_id}-{address}.{ext}` from the row itself.
    Own,
    /// Reuse another row's vendored icon file.
    Shared(&'static str),
    /// No vendored icon ships for this token.
    None,
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
    addr: ETH_BTC_ETH_FEED,
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
    addr: POLYGON_POL_USD_FEED,
    token_decimals: 6,
    oracle_decimals: 8,
    is_inversed: false,
}];
const MATIC_USD_18_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: POLYGON_POL_USD_FEED,
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
    addr: ARB_BTC_ETH_FEED,
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

const RAIL_ETH_TWAP_ANCHOR: &[TokenAnchorSource] = &[TokenAnchorSource::UniswapV3Twap {
    pool: address!("0x2837809FD68e4a4104af76bbec5b622b6146B2cb"),
    base_token: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
    quote_token: address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D"),
    base_token_decimals: 18,
    window_seconds: 1_800,
}];

const fn feed(
    addr: Address,
    oracle_decimals: u8,
    token_decimals: u8,
    is_inversed: bool,
) -> TokenAnchorSource {
    TokenAnchorSource::ChainlinkOracle {
        addr,
        token_decimals,
        oracle_decimals,
        is_inversed,
    }
}

// ---- Chainlink price feed proxies used by the built-in token anchors below ----
// Source: https://data.chain.link/feeds (reference-data-directory feeds-*.json), fetched 2026-09-21.
/// Arbitrum (42161) Chainlink `AAVE / USD`.
const ARB_AAVE_USD_FEED: Address = address!("0xaD1d5344AaDE45F43E596773Bcc4c423EAbdD034");
/// Arbitrum (42161) Chainlink `BTC / ETH`.
const ARB_BTC_ETH_FEED: Address = address!("0xc5a90A6d7e4Af242dA238FFe279e9f2BA0c64B2e");
/// Arbitrum (42161) Chainlink `CBETH / ETH`.
const ARB_CBETH_ETH_FEED: Address = address!("0xa668682974E3f121185a3cD94f00322beC674275");
/// Arbitrum (42161) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const ARB_ETH_USD_FEED: Address = address!("0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612");
/// Arbitrum (42161) Chainlink `LINK / ETH`.
const ARB_LINK_ETH_FEED: Address = address!("0xb7c8Fb1dB45007F98A68Da0588e1AA524C317f27");
/// Arbitrum (42161) Chainlink `RETH / ETH`.
const ARB_RETH_ETH_FEED: Address = address!("0xD6aB2298946840262FcC278fF31516D39fF611eF");
/// Arc (5042) Chainlink `ETH / USD`.
const ARC_ETH_USD_FEED: Address = address!("0x50FCDD99D6762D1C170DC6A9111db944AEE6D364");
/// Arc (5042) Chainlink `EURC / USD`.
const ARC_EURC_USD_FEED: Address = address!("0x361b95c10b76Ca3f35C686d423e43A951755Bf23");
/// Avalanche (43114) Chainlink `AAVE / USD`.
const AVAX_AAVE_USD_FEED: Address = address!("0x3CA13391E9fb38a75330fb28f8cc2eB3D9ceceED");
/// Avalanche (43114) Chainlink `AVAX / USD (same proxy as the chain preset's native oracle)`.
const AVAX_AVAX_USD_FEED: Address = address!("0x0A77230d17318075983913bC2145DB16C7366156");
/// Avalanche (43114) Chainlink `BTC / USD`.
const AVAX_BTC_USD_FEED: Address = address!("0x2779D32d5166BAaa2B2b658333bA7e6Ec0C65743");
/// Avalanche (43114) Chainlink `LINK / AVAX`.
const AVAX_LINK_AVAX_FEED: Address = address!("0x1b8a25F73c9420dD507406C3A3816A276b62f56a");
/// Avalanche (43114) Chainlink `UNI / USD`.
const AVAX_UNI_USD_FEED: Address = address!("0x9a1372f9b1B71B3A5a72E092AE67E172dBd7Daaa");
/// Avalanche (43114) Chainlink `WBTC / USD`.
const AVAX_WBTC_USD_FEED: Address = address!("0x86442E3a98558357d46E6182F4b262f76c4fa26F");
/// Base (8453) Chainlink `AAVE / USD`.
const BASE_AAVE_USD_FEED: Address = address!("0x3d6774EF702A10b20FCa8Ed40FC022f7E4938e07");
/// Base (8453) Chainlink `AERO / USD`.
const BASE_AERO_USD_FEED: Address = address!("0x4EC5970fC728C5f65ba413992CD5fF6FD70fcfF0");
/// Base (8453) Chainlink `cbBTC / USD`.
const BASE_CBBTC_USD_FEED: Address = address!("0x07DA0E54543a844a80ABE69c8A12F22B3aA59f9D");
/// Base (8453) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const BASE_ETH_USD_FEED: Address = address!("0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70");
/// Base (8453) Chainlink `LINK / ETH`.
const BASE_LINK_ETH_FEED: Address = address!("0xc5E65227fe3385B88468F9A01600017cDC9F3A12");
/// Base (8453) Chainlink `MORPHO / USD`.
const BASE_MORPHO_USD_FEED: Address = address!("0xe95e258bb6615d47515Fc849f8542dA651f12bF6");
/// Base (8453) Chainlink `RETH / ETH`.
const BASE_RETH_ETH_FEED: Address = address!("0xf397bF97280B488cA19ee3093E81C0a77F02e9a5");
/// Base (8453) Chainlink `WBTC / USD`.
const BASE_WBTC_USD_FEED: Address = address!("0xCCADC697c55bbB68dc5bCdf8d3CBe83CdD4E071E");
/// Base (8453) Chainlink `weETH / ETH`.
const BASE_WEETH_ETH_FEED: Address = address!("0xFC1415403EbB0c693f9a7844b92aD2Ff24775C65");
/// BSC (56) Chainlink `AAVE / USD`.
const BSC_AAVE_USD_FEED: Address = address!("0xA8357BF572460fC40f4B0aCacbB2a6A61c89f475");
/// BSC (56) Chainlink `BNB / USD (same proxy as the chain preset's native oracle)`.
const BSC_BNB_USD_FEED: Address = address!("0x0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE");
/// BSC (56) Chainlink `BTC / BNB`.
const BSC_BTC_BNB_FEED: Address = address!("0x116EeB23384451C78ed366D4f67D5AD44eE771A0");
/// BSC (56) Chainlink `LINK / BNB`.
const BSC_LINK_BNB_FEED: Address = address!("0xB38722F6A608646a538E882Ee9972D15c86Fc597");
/// BSC (56) Chainlink `UNI / BNB`.
const BSC_UNI_BNB_FEED: Address = address!("0x25298F020c3CA1392da76Eb7Ac844813b218ccf7");
/// Ethereum (1) Chainlink `AAVE / ETH`.
const ETH_AAVE_ETH_FEED: Address = address!("0x6Df09E975c830ECae5bd4eD9d90f3A95a4f88012");
/// Ethereum (1) Chainlink `BTC / ETH`.
const ETH_BTC_ETH_FEED: Address = address!("0xdeb288F737066589598e9214E782fa5A8eD689e8");
/// Ethereum (1) Chainlink `STETH / ETH`.
const ETH_STETH_ETH_FEED: Address = address!("0x86392dC19c0b719886221c78AB11eb8Cf5c52812");
/// Ethereum (1) Chainlink `UNI / ETH`.
const ETH_UNI_ETH_FEED: Address = address!("0xD6aA3D25116d8dA79Ea0246c4826EB951872e02e");
/// Gnosis (100) Chainlink `ETH / USD`.
const GNOSIS_ETH_USD_FEED: Address = address!("0xa767f745331D267c7751297D982b050c93985627");
/// Gnosis (100) Chainlink `GNO / USD`.
const GNOSIS_GNO_USD_FEED: Address = address!("0x22441d81416430A54336aB28765abd31a792Ad37");
/// Gnosis (100) Chainlink `LINK / USD`.
const GNOSIS_LINK_USD_FEED: Address = address!("0xed322A5ac55BAE091190dFf9066760b86751947B");
/// Gnosis (100) Chainlink `UNI / USD`.
const GNOSIS_UNI_USD_FEED: Address = address!("0xd98735d78266c62277Bb4dBf3e3bCdd3694782F4");
/// Gnosis (100) Chainlink `WBTC / USD`.
const GNOSIS_WBTC_USD_FEED: Address = address!("0x00288135bE38B83249F380e9b6b9a04c90EC39eE");
/// Ink (57073) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const INK_ETH_USD_FEED: Address = address!("0x963d5d3aD2Dfd3fe759d376fF62A0963176DBdF5");
/// Ink (57073) Chainlink `LINK / USD`.
const INK_LINK_USD_FEED: Address = address!("0xecaC179d2AD72e7624EF5257f876C612ff216bF2");
/// Katana (747474) Chainlink `BTC / USD`.
const KATANA_BTC_USD_FEED: Address = address!("0x41DdB7F8F5e1b2bD28193B84C1C36Be698dEd162");
/// Katana (747474) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const KATANA_ETH_USD_FEED: Address = address!("0x7BdBDB772f4a073BadD676A567C6ED82049a8eEE");
/// Katana (747474) Chainlink `KAT / USD`.
const KATANA_KAT_USD_FEED: Address = address!("0xc62782910529ee50eFDa9a0273B20d8bD1C1e4b2");
/// Katana (747474) Chainlink `LBTC / USD`.
const KATANA_LBTC_USD_FEED: Address = address!("0x5C2c6A77310C7750fCc5c3f13a3f9C3b18a68d3e");
/// Katana (747474) Chainlink `LINK / USD`.
const KATANA_LINK_USD_FEED: Address = address!("0x06bD6464e94Bee9393Ae15B5Dd5eCDFAa4F299C1");
/// Katana (747474) Chainlink `MORPHO / USD`.
const KATANA_MORPHO_USD_FEED: Address = address!("0xdFd824A5Dcad8667142d58FE4aF115d5d052f26c");
/// Katana (747474) Chainlink `weETH / ETH`.
const KATANA_WEETH_ETH_FEED: Address = address!("0x3Eae75C0a2f9b1038C7c9993C1Da36281E838811");
/// Linea (59144) Chainlink `BTC / USD`.
const LINEA_BTC_USD_FEED: Address = address!("0x7A99092816C8BD5ec8ba229e3a6E6Da1E628E1F9");
/// Linea (59144) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const LINEA_ETH_USD_FEED: Address = address!("0x3c6Cd9Cc7c7a4c2Cf5a82734CD249D7D593354dA");
/// Linea (59144) Chainlink `LINEA / USD`.
const LINEA_LINEA_USD_FEED: Address = address!("0x452b408b3e2805C109D52C80Bd54Deda239716d1");
/// Linea (59144) Chainlink `LINK / ETH`.
const LINEA_LINK_ETH_FEED: Address = address!("0xc4194f19E3a0836F6B998394445C6535c50604Ce");
/// Linea (59144) Chainlink `weETH / ETH`.
const LINEA_WEETH_ETH_FEED: Address = address!("0xC4bF21Ab46bd22Cf993c0AAa363577bD2Af83544");
/// Linea (59144) Chainlink `WSTETH / USD`.
const LINEA_WSTETH_USD_FEED: Address = address!("0x8eCE1AbA32716FdDe8D6482bfd88E9a0ee01f565");
/// Mantle (5000) Chainlink `BTC / USD`.
const MANTLE_BTC_USD_FEED: Address = address!("0x7db2275279F52D0914A481e14c4Ce5a59705A25b");
/// Mantle (5000) Chainlink `ETH / USD`.
const MANTLE_ETH_USD_FEED: Address = address!("0x5bc7Cf88EB131DB18b5d7930e793095140799aD5");
/// Mantle (5000) Chainlink `LINK / USD`.
const MANTLE_LINK_USD_FEED: Address = address!("0x5871AdBEEdAD531C68A8FD32fE86f07d6b4C645d");
/// Mantle (5000) Chainlink `MNT / USD (same proxy as the chain preset's native oracle)`.
const MANTLE_MNT_USD_FEED: Address = address!("0xD97F20bEbeD74e8144134C4b148fE93417dd0F96");
/// `MegaETH` (4326) Chainlink `BTC / USD`.
const MEGAETH_BTC_USD_FEED: Address = address!("0xc6E3007B597f6F5a6330d43053D1EF73cCbbE721");
/// `MegaETH` (4326) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const MEGAETH_ETH_USD_FEED: Address = address!("0xcA4e254D95637DE95E2a2F79244b03380d697feD");
/// `MegaETH` (4326) Chainlink `MEGA / USD`.
const MEGAETH_MEGA_USD_FEED: Address = address!("0x1F96c2fB10Ce4D48b6730A4766744E525cE42CAF");
/// Monad (143) Chainlink `CBBTC / USD`.
const MONAD_CBBTC_USD_FEED: Address = address!("0x3dDc1bAE752aaEe31b577bF844c799C349A1d6BD");
/// Monad (143) Chainlink `ETH / USD`.
const MONAD_ETH_USD_FEED: Address = address!("0x1B1414782B859871781bA3E4B0979b9ca57A0A04");
/// Monad (143) Chainlink `LINK / USD`.
const MONAD_LINK_USD_FEED: Address = address!("0x5c266b5c655664d6c99a13fF0d7F1F7eaF4Ac9ba");
/// Monad (143) Chainlink `MON / USD (same proxy as the chain preset's native oracle)`.
const MONAD_MON_USD_FEED: Address = address!("0xBcD78f76005B7515837af6b50c7C52BCf73822fb");
/// Monad (143) Chainlink `WBTC / USD`.
const MONAD_WBTC_USD_FEED: Address = address!("0x2D1Df1bD061AAc38C22407AD69d69bCC3C62edBD");
/// Monad (143) Chainlink `WEETH / USD`.
const MONAD_WEETH_USD_FEED: Address = address!("0x42dd36b9D6938dccff8Fe4E9770589aBa614FCBB");
/// Monad (143) Chainlink `WSTETH / USD`.
const MONAD_WSTETH_USD_FEED: Address = address!("0xe6cd21b31948503dB54A07875999979722504B9A");
/// Optimism (10) Chainlink `AAVE / USD`.
const OP_AAVE_USD_FEED: Address = address!("0x338ed6787f463394D24813b297401B9F05a8C9d1");
/// Optimism (10) Chainlink `BTC / USD`.
const OP_BTC_USD_FEED: Address = address!("0xD702DD976Fb76Fffc2D3963D037dfDae5b04E593");
/// Optimism (10) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const OP_ETH_USD_FEED: Address = address!("0x13e3Ee699D1909E989722E753853AE30b17e08c5");
/// Optimism (10) Chainlink `LINK / ETH`.
const OP_LINK_ETH_FEED: Address = address!("0x464A1515ADc20de946f8d0DEB99cead8CEAE310d");
/// Optimism (10) Chainlink `OP / USD`.
const OP_OP_USD_FEED: Address = address!("0x0D276FC14719f9292D5C1eA2198673d1f4269246");
/// Optimism (10) Chainlink `RETH / ETH`.
const OP_RETH_ETH_FEED: Address = address!("0xb429DE60943a8e6DeD356dca2F93Cd31201D9ed0");
/// Optimism (10) Chainlink `UNI / USD`.
const OP_UNI_USD_FEED: Address = address!("0x11429eE838cC01071402f21C219870cbAc0a59A0");
/// Optimism (10) Chainlink `WBTC / USD`.
const OP_WBTC_USD_FEED: Address = address!("0x718A5788b89454aAE3A028AE9c111A29Be6c2a6F");
/// Optimism (10) Chainlink `weETH / ETH`.
const OP_WEETH_ETH_FEED: Address = address!("0xb4479d436DDa5c1A79bD88D282725615202406E3");
/// Plasma (9745) Chainlink `ETH / USD`.
const PLASMA_ETH_USD_FEED: Address = address!("0x43A7dd2125266c5c4c26EB86cd61241132426Fe7");
/// Plasma (9745) Chainlink `LINK / USD`.
const PLASMA_LINK_USD_FEED: Address = address!("0xe37F74Cb2237C5274DDbBf841C3284Bd2E23E65B");
/// Plasma (9745) Chainlink `WEETH / USD`.
const PLASMA_WEETH_USD_FEED: Address = address!("0xBfEd4ef33B8ec58bC15fcbb4a0f934A812d5D7b5");
/// Plasma (9745) Chainlink `XPL / USD (same proxy as the chain preset's native oracle)`.
const PLASMA_XPL_USD_FEED: Address = address!("0xF932477C37715aE6657Ab884414Bd9876FE3f750");
/// Polygon (137) Chainlink `AAVE / USD`.
const POLYGON_AAVE_USD_FEED: Address = address!("0x72484B12719E23115761D5DA1646945632979bB6");
/// Polygon (137) Chainlink `BTC / USD`.
const POLYGON_BTC_USD_FEED: Address = address!("0xc907E116054Ad103354f2D350FD2514433D57F6f");
/// Polygon (137) Chainlink `LINK / USD`.
const POLYGON_LINK_USD_FEED: Address = address!("0xd9FFdb71EbE7496cC440152d43986Aae0AB76665");
/// Polygon (137) Chainlink `UNI / USD`.
const POLYGON_UNI_USD_FEED: Address = address!("0xdf0Fb4e4F928d2dCB76f438575fDD8682386e13C");
/// Robinhood Chain (4663) Chainlink `CBBTC / USD`.
const ROBINHOOD_CBBTC_USD_FEED: Address = address!("0x0009cD492adf8167f9eEBf1293556A673530a21a");
/// Robinhood Chain (4663) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const ROBINHOOD_ETH_USD_FEED: Address = address!("0x78F3556b67E17Df817D51Ef5a990cDaF09E8d3A9");
/// Robinhood Chain (4663) Chainlink `LINK / USD`.
const ROBINHOOD_LINK_USD_FEED: Address = address!("0xe86e3422Aa9B5e8ee9f3E41a63975bC387A8bce9");
/// Sonic (146) Chainlink `BTC / USD`.
const SONIC_BTC_USD_FEED: Address = address!("0x8Bcd59Cb7eEEea8e2Da3080C891609483dae53EF");
/// Sonic (146) Chainlink `ETH / USD`.
const SONIC_ETH_USD_FEED: Address = address!("0x824364077993847f71293B24ccA8567c00c2de11");
/// Sonic (146) Chainlink `LINK / USD`.
const SONIC_LINK_USD_FEED: Address = address!("0x26e450ca14D7bF598C89f212010c691434486119");
/// Sonic (146) Chainlink `S / USD (same proxy as the chain preset's native oracle)`.
const SONIC_S_USD_FEED: Address = address!("0xc76dFb89fF298145b417d221B2c747d84952e01d");
/// Sonic (146) Chainlink `WBTC / USD`.
const SONIC_WBTC_USD_FEED: Address = address!("0x61140C09956495F1ce49D28E125Ed4035e1CAE95");
/// Unichain (130) Chainlink `BTC / USD`.
const UNICHAIN_BTC_USD_FEED: Address = address!("0xC13f3E310Dd7436FA24338174acB64254b9A8039");
/// Unichain (130) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const UNICHAIN_ETH_USD_FEED: Address = address!("0xe8D9FbC10e00ecc9f0694617075fDAF657a76FB2");
/// Unichain (130) Chainlink `LINK / USD`.
const UNICHAIN_LINK_USD_FEED: Address = address!("0x04343180ABa8543a850A87d594644D84fE38a919");
/// Unichain (130) Chainlink `UNI / USD`.
const UNICHAIN_UNI_USD_FEED: Address = address!("0xdAd6f90429a2C821496B78Fe7482412971E278f1");
/// Berachain (80094) Chainlink `BERA / USD (same proxy as the chain preset's native oracle)`.
const BERACHAIN_BERA_USD_FEED: Address = address!("0x29d2fEC890B037B2d34f061F9a50f76F85ddBcAE");
/// Blast (81457) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const BLAST_ETH_USD_FEED: Address = address!("0x458AD5B487F4442245E4C5eA7249009E607A5583");
/// Cronos (25) Chainlink `CRO / USD (same proxy as the chain preset's native oracle)`.
const CRONOS_CRO_USD_FEED: Address = address!("0x4636AC8216805Fe96dE9E7aFc62dA99096a930F6");
/// Etherlink (42793) Chainlink `XTZ / USD (same proxy as the chain preset's native oracle)`.
const ETHERLINK_XTZ_USD_FEED: Address = address!("0x929dB17A4673f150251fDc7AC4E7B5dd7b2Fd654");
/// Fraxtal (252) Chainlink `FRAX / USD (same proxy as the chain preset's native oracle)`.
const FRAXTAL_FRAX_USD_FEED: Address = address!("0xbf228a9131AB3BB8ca8C7a4Ad574932253D99Cd1");
/// Gnosis (100) Chainlink `XDAI / USD (same proxy as the chain preset's native oracle)`.
const GNOSIS_XDAI_USD_FEED: Address = address!("0xE5269eF0CE04E509E8134624c7BF043b21e10897");
/// Pharos (1672) Chainlink `PROS / USD (same proxy as the chain preset's native oracle)`.
const PHAROS_PROS_USD_FEED: Address = address!("0x9356c87a48f913d11c87a0d4b8cd16cd04624bf3");
/// Polygon (137) Chainlink `POL / USD (same proxy as the chain preset's native oracle)`.
const POLYGON_POL_USD_FEED: Address = address!("0xAB594600376Ec9fD91F8e885dADF0CE036862dE0");
/// World Chain (480) Chainlink `ETH / USD (same proxy as the chain preset's native oracle)`.
const WORLDCHAIN_ETH_USD_FEED: Address = address!("0x9d5d754a6397c01795Aaa8BBA565FEB99a4cEA6d");
/// `HyperEVM` (999) Chainlink-compatible `HYPE / USD (same proxy as the chain preset's native oracle)`.
const HYPEREVM_HYPE_USD_FEED: Address = address!("0xa8a94Da411425634e3Ed6C331a32ab4fd774aa43");

/// Chain id, token address, symbol, decimals, fee anchors, icon.
type TokenRow = (
    u64,
    Address,
    &'static str,
    u8,
    &'static [TokenAnchorSource],
    TokenIcon,
);

#[rustfmt::skip]
const TOKENS: &[TokenRow] = &[
    // Ethereum (1)
    (1, address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), "WETH", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::Own),
    (1, address!("0xdAC17F958D2ee523a2206206994597C13D831ec7"), "USDT", 6, ETH_USD_6_ANCHOR, TokenIcon::Own),
    (1, address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), "USDC", 6, ETH_USD_6_ANCHOR, TokenIcon::Own),
    (1, address!("0x6b175474e89094c44da98b954eedeac495271d0f"), "DAI", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"), "WBTC", 8, BTC_ETH_8_ANCHOR, TokenIcon::Own),
    (1, address!("0x1aBaEA1f7C830bD89Acc67eC4af516284b1bC33c"), "EURC", 6, NO_ANCHORS, TokenIcon::Own),
    (1, address!("0x6f40d4a6237c257fff2db00fa0510deeecd303eb"), "FLUID", 18, NO_ANCHORS, TokenIcon::Own),
    (1, address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D"), "RAIL", 18, RAIL_ETH_TWAP_ANCHOR, TokenIcon::Own),
    (1, address!("0x03ab458634910aad20ef5f1c8ee96f1d6ac54919"), "RAI", 18, NO_ANCHORS, TokenIcon::Own),
    (1, address!("0x853d955aCEf822Db058eb8505911ED77F175b99e"), "FRAX", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0x956f47f50a910163d8bf957cf5846d573e7f87ca"), "FEI", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0xeb4c2781e4eba804ce9a9803c67d0893436bb27d"), "renBTC", 8, BTC_ETH_8_ANCHOR, TokenIcon::Own),
    (1, address!("0x085780639CC2cACd35E474e71f4d000e2405d8f6"), "fxUSD", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0x4c9EDD5852cd905f086C759E8383e09bff1E68B3"), "USDe", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0x4f8e5DE400DE08B164E7421B3EE387f461beCD1A"), "USDD", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0x8d0D000Ee44948FC98c9B98A4FA4921476f08B0d"), "USD1", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0xdC035D45d973E3EC169d2276DDab16f1e407384F"), "USDS", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0xe343167631d89B6Ffc58B88d6b7fB0228795491D"), "USDG", 6, ETH_USD_6_ANCHOR, TokenIcon::Own),
    (1, address!("0xFa2B947eEc368f42195f24F36d2aF29f7c24CeC2"), "USDF", 18, ETH_USD_18_ANCHOR, TokenIcon::Own),
    (1, address!("0x514910771AF9Ca656af840dff83E8264EcF986CA"), "LINK", 18, NO_ANCHORS, TokenIcon::Own),
    (1, address!("0x6c3ea9036406852006290770BEdFcAbA0e23A0e8"), "PYUSD", 6, ETH_USD_6_ANCHOR, TokenIcon::Own),
    (1, address!("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf"), "cbBTC", 8, &[feed(ETH_BTC_ETH_FEED, 18, 8, true)], TokenIcon::None),
    (1, address!("0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84"), "stETH", 18, &[feed(ETH_STETH_ETH_FEED, 18, 18, true)], TokenIcon::None),
    (1, address!("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"), "wstETH", 18, NO_ANCHORS, TokenIcon::None),
    (1, address!("0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984"), "UNI", 18, &[feed(ETH_UNI_ETH_FEED, 18, 18, true)], TokenIcon::Shared("42161-0xfa7f8980b0f1e64a2062791cc3b0871572f1f7f0.png")),
    (1, address!("0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9"), "AAVE", 18, &[feed(ETH_AAVE_ETH_FEED, 18, 18, true)], TokenIcon::None),
    // BSC (56)
    (56, address!("0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c"), "WBNB", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::Own),
    (56, address!("0x55d398326f99059ff775485246999027b3197955"), "BSC-USD", 18, BNB_USD_18_ANCHOR, TokenIcon::Own),
    (56, address!("0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d"), "USDC", 18, BNB_USD_18_ANCHOR, TokenIcon::Own),
    (56, address!("0xe9e7cea3dedca5984780bafc599bd69add087d56"), "BUSD", 18, BNB_USD_18_ANCHOR, TokenIcon::Own),
    (56, address!("0x1af3f329e8be154074d8769d1ffa4ee058b1dbc3"), "DAI", 18, BNB_USD_18_ANCHOR, TokenIcon::Own),
    (56, address!("0x0E09FaBB73Bd3Ade0a17ECC321fD13a19e81cE82"), "CAKE", 18, NO_ANCHORS, TokenIcon::Own),
    (56, address!("0x2170Ed0880ac9A755fd29B2688956BD959F933F8"), "ETH", 18, NO_ANCHORS, TokenIcon::Own),
    (56, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(BSC_BNB_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (56, address!("0x8d0D000Ee44948FC98c9B98A4FA4921476f08B0d"), "USD1", 18, &[feed(BSC_BNB_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x8d0d000ee44948fc98c9b98a4fa4921476f08b0d.png")),
    (56, address!("0x0555E30da8f98308EdB960aa94C0Db47230d2B9c"), "WBTC", 8, &[feed(BSC_BTC_BNB_FEED, 18, 8, true)], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (56, address!("0xecAc9C5F704e954931349Da37F60E39f515c11c1"), "LBTC", 8, &[feed(BSC_BTC_BNB_FEED, 18, 8, true)], TokenIcon::None),
    (56, address!("0x04C0599Ae5A44757c0af6F9eC3b93da8976c150A"), "weETH", 18, NO_ANCHORS, TokenIcon::None),
    (56, address!("0xF8A0BF9cF54Bb92F17374d9e9A321E6a111a51bD"), "LINK", 18, &[feed(BSC_LINK_BNB_FEED, 18, 18, true)], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (56, address!("0xBf5140A22578168FD562DCcF235E5D43A02ce9B1"), "UNI", 18, &[feed(BSC_UNI_BNB_FEED, 18, 18, true)], TokenIcon::Shared("42161-0xfa7f8980b0f1e64a2062791cc3b0871572f1f7f0.png")),
    (56, address!("0xfb6115445Bff7b52FeB98650C87f44907E58f802"), "AAVE", 18, &[TokenAnchorSource::Product { sources: &[feed(BSC_BNB_USD_FEED, 8, 18, false), feed(BSC_AAVE_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Polygon (137)
    (137, address!("0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270"), "WMATIC", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::Own),
    (137, address!("0xc2132d05d31c914a87c6611c10748aeb04b58e8f"), "USDT", 6, MATIC_USD_6_ANCHOR, TokenIcon::Own),
    (137, address!("0x2791bca1f2de4661ed88a30c99a7a9449aa84174"), "USDC.e", 6, MATIC_USD_6_ANCHOR, TokenIcon::Own),
    (137, address!("0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359"), "USDC", 6, MATIC_USD_6_ANCHOR, TokenIcon::Own),
    (137, address!("0x8f3cf7ad23cd3cadbd9735aff958023239c6a063"), "DAI", 18, MATIC_USD_18_ANCHOR, TokenIcon::Own),
    (137, address!("0x1BFD67037B42Cf73acF2047067bd4F2C47D9BfD6"), "WBTC", 8, NO_ANCHORS, TokenIcon::Own),
    (137, address!("0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619"), "WETH", 18, NO_ANCHORS, TokenIcon::Own),
    (137, address!("0x99aF3EeA856556646C98c8B9b2548Fe815240750"), "PYUSD", 6, &[feed(POLYGON_POL_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0x6c3ea9036406852006290770bedfcaba0e23a0e8.png")),
    (137, address!("0x236aa50979D5f3De3Bd1Eeb40E81137F22ab794b"), "tBTC", 18, &[TokenAnchorSource::Product { sources: &[feed(POLYGON_POL_USD_FEED, 8, 18, false), feed(POLYGON_BTC_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (137, address!("0x0266F4F08D82372CF0FcbCCc0Ff74309089c74d1"), "rETH", 18, NO_ANCHORS, TokenIcon::None),
    (137, address!("0x53E0bca35eC356BD5ddDFebbD1Fc0fD03FaBad39"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(POLYGON_POL_USD_FEED, 8, 18, false), feed(POLYGON_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (137, address!("0xb33EaAd8d922B1083446DC23f610c2567fB5180f"), "UNI", 18, &[TokenAnchorSource::Product { sources: &[feed(POLYGON_POL_USD_FEED, 8, 18, false), feed(POLYGON_UNI_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("42161-0xfa7f8980b0f1e64a2062791cc3b0871572f1f7f0.png")),
    (137, address!("0xD6DF932A45C0f255f85145f286eA0b292B21C90B"), "AAVE", 18, &[TokenAnchorSource::Product { sources: &[feed(POLYGON_POL_USD_FEED, 8, 18, false), feed(POLYGON_AAVE_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (137, address!("0x0000000000000000000000000000000000001010"), "POL", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::Shared("137-0x0d500b1d8e8ef31e21c99d1db9a6444d3adf1270.png")),
    // Arbitrum (42161)
    (42161, address!("0x82af49447d8a07e3bd95bd0d56f35241523fbab1"), "WETH", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::Own),
    (42161, address!("0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9"), "USDT", 6, ARB_ETH_USD_6_ANCHOR, TokenIcon::Own),
    (42161, address!("0xff970a61a04b1ca14834a43f5de4533ebddb5cc8"), "USDC.e", 6, ARB_ETH_USD_6_ANCHOR, TokenIcon::Own),
    (42161, address!("0xaf88d065e77c8cc2239327c5edb3a432268e5831"), "USDC", 6, ARB_ETH_USD_6_ANCHOR, TokenIcon::Own),
    (42161, address!("0xda10009cbd5d07dd0cecc66161fc93d7c9000da1"), "DAI", 18, ARB_ETH_USD_18_ANCHOR, TokenIcon::Own),
    (42161, address!("0x2f2a2543b76a4166549f7aab2e75bef0aefc5b0f"), "WBTC", 8, ARB_BTC_ETH_8_ANCHOR, TokenIcon::Own),
    (42161, address!("0x912ce59144191c1204e64559fe8253a0e49e6548"), "ARB", 18, ARB_PER_ETH_18_ANCHOR, TokenIcon::Own),
    (42161, address!("0xFa7F8980b0f1E64A2062791cc3b0871572f1F7f0"), "UNI", 18, NO_ANCHORS, TokenIcon::Own),
    (42161, address!("0x17FC002b466eEc40DaE837Fc4bE5c67993ddBd6F"), "FRAX", 18, ARB_ETH_USD_18_ANCHOR, TokenIcon::Own),
    (42161, address!("0x4D15a3A2286D883AF0AA1B3f21367843FAc63E07"), "TUSD", 18, ARB_ETH_USD_18_ANCHOR, TokenIcon::Own),
    (42161, address!("0x6491c05A82219b8D1479057361ff1654749b876b"), "USDS", 18, &[feed(ARB_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0xdc035d45d973e3ec169d2276ddab16f1e407384f.png")),
    (42161, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(ARB_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (42161, address!("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf"), "cbBTC", 8, &[feed(ARB_BTC_ETH_FEED, 18, 8, true)], TokenIcon::None),
    (42161, address!("0xC96dE26018A54D51c097160568752c4E3BD6C364"), "FBTC", 8, &[feed(ARB_BTC_ETH_FEED, 18, 8, true)], TokenIcon::None),
    (42161, address!("0xEC70Dcb4A1EFa46b8F2D97C310C9c4790ba5ffA8"), "rETH", 18, &[feed(ARB_RETH_ETH_FEED, 18, 18, true)], TokenIcon::None),
    (42161, address!("0x1DEBd73E752bEaF79865Fd6446b0c970EaE7732f"), "cbETH", 18, &[feed(ARB_CBETH_ETH_FEED, 18, 18, true)], TokenIcon::None),
    (42161, address!("0xf97f4df75117a78c1A5a0DBb814Af92458539FB4"), "LINK", 18, &[feed(ARB_LINK_ETH_FEED, 18, 18, true)], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (42161, address!("0xba5DdD1f9d7F570dc94a51479a000E3BCE967196"), "AAVE", 18, &[TokenAnchorSource::Product { sources: &[feed(ARB_ETH_USD_FEED, 8, 18, false), feed(ARB_AAVE_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Avalanche (43114)
    (43114, address!("0x9702230A8Ea53601f5cD2dc00fDBc13d4dF4A8c7"), "USDt", 6, &[feed(AVAX_AVAX_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (43114, address!("0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E"), "USDC", 6, &[feed(AVAX_AVAX_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (43114, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(AVAX_AVAX_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (43114, address!("0x0555E30da8f98308EdB960aa94C0Db47230d2B9c"), "WBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(AVAX_AVAX_USD_FEED, 8, 8, false), feed(AVAX_WBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (43114, address!("0x152b9d0FdC40C096757F570A51E494bd4b943E50"), "BTC.b", 8, &[TokenAnchorSource::Product { sources: &[feed(AVAX_AVAX_USD_FEED, 8, 8, false), feed(AVAX_BTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (43114, address!("0xA3D68b74bF0528fdD07263c60d6488749044914b"), "weETH", 18, NO_ANCHORS, TokenIcon::None),
    (43114, address!("0x5947BB275c521040051D82396192181b413227A3"), "LINK.e", 18, &[feed(AVAX_LINK_AVAX_FEED, 18, 18, true)], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (43114, address!("0x8eBAf22B6F053dFFeaf46f4Dd9eFA95D89ba8580"), "UNI.e", 18, &[TokenAnchorSource::Product { sources: &[feed(AVAX_AVAX_USD_FEED, 8, 18, false), feed(AVAX_UNI_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("42161-0xfa7f8980b0f1e64a2062791cc3b0871572f1f7f0.png")),
    (43114, address!("0x63a72806098Bd3D9520cC43356dD78afe5D386D9"), "AAVE.e", 18, &[TokenAnchorSource::Product { sources: &[feed(AVAX_AVAX_USD_FEED, 8, 18, false), feed(AVAX_AAVE_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Base (8453)
    (8453, address!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"), "USDC", 6, &[feed(BASE_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (8453, address!("0x820C137fa70C8691f0e44Dc420a5e53c168921Dc"), "USDS", 18, &[feed(BASE_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0xdc035d45d973e3ec169d2276ddab16f1e407384f.png")),
    (8453, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(BASE_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (8453, address!("0x0555E30da8f98308EdB960aa94C0Db47230d2B9c"), "WBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(BASE_ETH_USD_FEED, 8, 8, false), feed(BASE_WBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (8453, address!("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf"), "cbBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(BASE_ETH_USD_FEED, 8, 8, false), feed(BASE_CBBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (8453, address!("0x04C0599Ae5A44757c0af6F9eC3b93da8976c150A"), "weETH", 18, &[feed(BASE_WEETH_ETH_FEED, 18, 18, true)], TokenIcon::None),
    (8453, address!("0xB6fe221Fe9EeF5aBa221c348bA20A1Bf5e73624c"), "rETH", 18, &[feed(BASE_RETH_ETH_FEED, 18, 18, true)], TokenIcon::None),
    (8453, address!("0x88Fb150BDc53A65fe94Dea0c9BA0a6dAf8C6e196"), "LINK", 18, &[feed(BASE_LINK_ETH_FEED, 18, 18, true)], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (8453, address!("0x63706e401c06ac8513145b7687A14804d17f814b"), "AAVE", 18, &[TokenAnchorSource::Product { sources: &[feed(BASE_ETH_USD_FEED, 8, 18, false), feed(BASE_AAVE_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (8453, address!("0x58538e6A46E07434d7E7375Bc268D3cb839C0133"), "ENA", 18, NO_ANCHORS, TokenIcon::None),
    (8453, address!("0xBAa5CC21fd487B8Fcc2F632f3F4E8D37262a0842"), "MORPHO", 18, &[TokenAnchorSource::Product { sources: &[feed(BASE_ETH_USD_FEED, 8, 18, false), feed(BASE_MORPHO_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (8453, address!("0x940181a94A35A4569E4529A3CDfB74e38FD98631"), "AERO", 18, &[TokenAnchorSource::Product { sources: &[feed(BASE_ETH_USD_FEED, 8, 18, false), feed(BASE_AERO_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Optimism (10)
    (10, address!("0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85"), "USDC", 6, &[feed(OP_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (10, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(OP_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (10, address!("0x01bFF41798a0BcF287b996046Ca68b395DbC1071"), "USDT0", 6, &[feed(OP_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (10, address!("0x68f180fcCe6836688e9084f035309E29Bf0A2095"), "WBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(OP_ETH_USD_FEED, 8, 8, false), feed(OP_WBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (10, address!("0x6c84a8f1c29108F47a79964b5Fe888D4f4D0dE40"), "tBTC", 18, &[TokenAnchorSource::Product { sources: &[feed(OP_ETH_USD_FEED, 8, 18, false), feed(OP_BTC_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (10, address!("0x5A7fACB970D094B6C7FF1df0eA68D99E6e73CBFF"), "weETH", 18, &[feed(OP_WEETH_ETH_FEED, 18, 18, true)], TokenIcon::None),
    (10, address!("0x9Bcef72be871e61ED4fBbc7630889beE758eb81D"), "rETH", 18, &[feed(OP_RETH_ETH_FEED, 18, 18, true)], TokenIcon::None),
    (10, address!("0x350a791Bfc2C21F9Ed5d10980Dad2e2638ffa7f6"), "LINK", 18, &[feed(OP_LINK_ETH_FEED, 18, 18, true)], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (10, address!("0x6fd9d7AD17242c41f7131d257212c54A0e816691"), "UNI", 18, &[TokenAnchorSource::Product { sources: &[feed(OP_ETH_USD_FEED, 8, 18, false), feed(OP_UNI_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("42161-0xfa7f8980b0f1e64a2062791cc3b0871572f1f7f0.png")),
    (10, address!("0x76FB31fb4af56892A25e32cFC43De717950c9278"), "AAVE", 18, &[TokenAnchorSource::Product { sources: &[feed(OP_ETH_USD_FEED, 8, 18, false), feed(OP_AAVE_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (10, address!("0x4200000000000000000000000000000000000042"), "OP", 18, &[TokenAnchorSource::Product { sources: &[feed(OP_ETH_USD_FEED, 8, 18, false), feed(OP_OP_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Gnosis (100)
    (100, address!("0xfc421aD3C883Bf9E7C4f42dE845C4e4405799e73"), "GHO", 18, &[feed(GNOSIS_XDAI_USD_FEED, 8, 18, false)], TokenIcon::None),
    (100, address!("0xe91D153E0b41518A2Ce8Dd3D7944Fa863463a97d"), "WXDAI", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    (100, address!("0xDDAfbb505ad214D7b80b1f830fcCc89B60fb7A83"), "USDC", 6, &[feed(GNOSIS_XDAI_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (100, address!("0x8e5bBbb09Ed1ebdE8674Cda39A0c169401db4252"), "WBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(GNOSIS_XDAI_USD_FEED, 8, 8, false), feed(GNOSIS_WBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (100, address!("0x6C76971f98945AE98dD7d4DFcA8711ebea946eA6"), "wstETH", 18, NO_ANCHORS, TokenIcon::None),
    (100, address!("0x6A023CCd1ff6F2045C3309768eAd9E68F978f6e1"), "WETH", 18, &[TokenAnchorSource::Product { sources: &[feed(GNOSIS_XDAI_USD_FEED, 8, 18, false), feed(GNOSIS_ETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (100, address!("0xE2e73A1c69ecF83F464EFCE6A5be353a37cA09b2"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(GNOSIS_XDAI_USD_FEED, 8, 18, false), feed(GNOSIS_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (100, address!("0x4537e328Bf7e4eFA29D05CAeA260D7fE26af9D74"), "UNI", 18, &[TokenAnchorSource::Product { sources: &[feed(GNOSIS_XDAI_USD_FEED, 8, 18, false), feed(GNOSIS_UNI_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("42161-0xfa7f8980b0f1e64a2062791cc3b0871572f1f7f0.png")),
    (100, address!("0x9C58BAcC331c9aa871AFD802DB6379a98e80CEdb"), "GNO", 18, &[TokenAnchorSource::Product { sources: &[feed(GNOSIS_XDAI_USD_FEED, 8, 18, false), feed(GNOSIS_GNO_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Mantle (5000)
    (5000, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(MANTLE_MNT_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (5000, address!("0x111111d2bf19e43C34263401e0CAd979eD1cdb61"), "USD1", 18, &[feed(MANTLE_MNT_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x8d0d000ee44948fc98c9b98a4fa4921476f08b0d.png")),
    (5000, address!("0x779Ded0c9e1022225f8E0630b35a9b54bE713736"), "USDT0", 6, &[feed(MANTLE_MNT_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (5000, address!("0xC96dE26018A54D51c097160568752c4E3BD6C364"), "FBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(MANTLE_MNT_USD_FEED, 8, 8, false), feed(MANTLE_BTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (5000, address!("0xdEAddEaDdeadDEadDEADDEAddEADDEAddead1111"), "WETH", 18, &[TokenAnchorSource::Product { sources: &[feed(MANTLE_MNT_USD_FEED, 8, 18, false), feed(MANTLE_ETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (5000, address!("0xfe36cF0B43aAe49fBc5cFC5c0AF22a623114E043"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(MANTLE_MNT_USD_FEED, 8, 18, false), feed(MANTLE_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (5000, address!("0x58538e6A46E07434d7E7375Bc268D3cb839C0133"), "ENA", 18, NO_ANCHORS, TokenIcon::None),
    (5000, address!("0xDeadDeAddeAddEAddeadDEaDDEAdDeaDDeAD0000"), "MNT", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    // Sonic (146)
    (146, address!("0x29219dd400f2Bf60E5a23d13Be72B486D4038894"), "USDC", 6, &[feed(SONIC_S_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (146, address!("0x80Eede496655FB9047dd39d9f418d5483ED600df"), "frxUSD", 18, &[feed(SONIC_S_USD_FEED, 8, 18, false)], TokenIcon::None),
    (146, address!("0x6047828dc181963ba44974801FF68e538dA5eaF9"), "USDT", 6, &[feed(SONIC_S_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (146, address!("0x0555E30da8f98308EdB960aa94C0Db47230d2B9c"), "WBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(SONIC_S_USD_FEED, 8, 8, false), feed(SONIC_WBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (146, address!("0xecAc9C5F704e954931349Da37F60E39f515c11c1"), "LBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(SONIC_S_USD_FEED, 8, 8, false), feed(SONIC_BTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (146, address!("0xA3D68b74bF0528fdD07263c60d6488749044914b"), "weETH", 18, NO_ANCHORS, TokenIcon::None),
    (146, address!("0x50c42dEAcD8Fc9773493ED674b675bE577f2634b"), "WETH", 18, &[TokenAnchorSource::Product { sources: &[feed(SONIC_S_USD_FEED, 8, 18, false), feed(SONIC_ETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (146, address!("0x71052BAe71C25C78E37fD12E5ff1101A71d9018F"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(SONIC_S_USD_FEED, 8, 18, false), feed(SONIC_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (146, address!("0xf1eF7d2D4C0c881cd634481e0586ed5d2871A74B"), "PENDLE", 18, NO_ANCHORS, TokenIcon::None),
    // Unichain (130)
    (130, address!("0x078D782b760474a361dDA0AF3839290b0EF57AD6"), "USDC", 6, &[feed(UNICHAIN_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (130, address!("0x9151434b16b9763660705744891fA906F660EcC5"), "USDT0", 6, &[feed(UNICHAIN_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (130, address!("0x80Eede496655FB9047dd39d9f418d5483ED600df"), "frxUSD", 18, &[feed(UNICHAIN_ETH_USD_FEED, 8, 18, false)], TokenIcon::None),
    (130, address!("0x0555E30da8f98308EdB960aa94C0Db47230d2B9c"), "WBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(UNICHAIN_ETH_USD_FEED, 8, 8, false), feed(UNICHAIN_BTC_USD_FEED, 18, 8, true)], scale_decimals: 8 }], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (130, address!("0xc02fE7317D4eb8753a02c35fe019786854A92001"), "wstETH", 18, NO_ANCHORS, TokenIcon::None),
    (130, address!("0x7DCC39B4d1C53CB31e1aBc0e358b43987FEF80f7"), "weETH", 18, NO_ANCHORS, TokenIcon::None),
    (130, address!("0xEF66491eab4bbB582c57b14778afd8dFb70D8A1A"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(UNICHAIN_ETH_USD_FEED, 8, 18, false), feed(UNICHAIN_LINK_USD_FEED, 18, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (130, address!("0x8f187aA05619a017077f5308904739877ce9eA21"), "UNI", 18, &[TokenAnchorSource::Product { sources: &[feed(UNICHAIN_ETH_USD_FEED, 8, 18, false), feed(UNICHAIN_UNI_USD_FEED, 18, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("42161-0xfa7f8980b0f1e64a2062791cc3b0871572f1f7f0.png")),
    // Ink (57073)
    (57073, address!("0x2D270e6886d130D724215A266106e6832161EAEd"), "USDC", 6, &[feed(INK_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (57073, address!("0x0200C29006150606B650577BBE7B6248F58470c1"), "USDT0", 6, &[feed(INK_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (57073, address!("0x142cdc44890978B506e745bB3Bd11607B7f7faEf"), "PYUSD", 6, &[feed(INK_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0x6c3ea9036406852006290770bedfcaba0e23a0e8.png")),
    (57073, address!("0xA3D68b74bF0528fdD07263c60d6488749044914b"), "weETH", 18, NO_ANCHORS, TokenIcon::None),
    (57073, address!("0x71052BAe71C25C78E37fD12E5ff1101A71d9018F"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(INK_ETH_USD_FEED, 8, 18, false), feed(INK_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    // Fraxtal (252)
    (252, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(FRAXTAL_FRAX_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (252, address!("0xFc00000000000000000000000000000000000001"), "frxUSD", 18, &[feed(FRAXTAL_FRAX_USD_FEED, 8, 18, false)], TokenIcon::None),
    (252, address!("0xDcc0F2D8F90FDe85b10aC1c8Ab57dc0AE946A543"), "USDC", 6, &[feed(FRAXTAL_FRAX_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (252, address!("0xd6A6ba37fAaC229B9665E86739ca501401f5a940"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (252, address!("0x58538e6A46E07434d7E7375Bc268D3cb839C0133"), "ENA", 18, NO_ANCHORS, TokenIcon::None),
    (252, address!("0xFc00000000000000000000000000000000000002"), "WFRAX", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::Shared("1-0x853d955acef822db058eb8505911ed77f175b99e.png")),
    // Blast (81457)
    (81457, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(BLAST_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (81457, address!("0x80Eede496655FB9047dd39d9f418d5483ED600df"), "frxUSD", 18, &[feed(BLAST_ETH_USD_FEED, 8, 18, false)], TokenIcon::None),
    (81457, address!("0x4300000000000000000000000000000000000003"), "USDB", 18, &[feed(BLAST_ETH_USD_FEED, 8, 18, false)], TokenIcon::None),
    (81457, address!("0x04C0599Ae5A44757c0af6F9eC3b93da8976c150A"), "weETH", 18, NO_ANCHORS, TokenIcon::None),
    (81457, address!("0x93202eC683288a9EA75BB829c6baCFb2BfeA9013"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (81457, address!("0x58538e6A46E07434d7E7375Bc268D3cb839C0133"), "ENA", 18, NO_ANCHORS, TokenIcon::None),
    (81457, address!("0xb1a5700fA2358173Fe465e6eA4Ff52E36e88E2ad"), "BLAST", 18, NO_ANCHORS, TokenIcon::None),
    // Linea (59144)
    (59144, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(LINEA_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (59144, address!("0xC7346783f5e645aa998B106Ef9E7f499528673D8"), "frxUSD", 18, &[feed(LINEA_ETH_USD_FEED, 8, 18, false)], TokenIcon::None),
    (59144, address!("0x176211869cA2b568f2A7D4EE941E073a821EE1ff"), "USDC", 6, &[feed(LINEA_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (59144, address!("0x3aAB2285ddcDdaD8edf438C1bAB47e1a9D05a9b4"), "WBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(LINEA_ETH_USD_FEED, 8, 8, false), feed(LINEA_BTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (59144, address!("0x1Bf74C010E6320bab11e2e5A532b5AC15e0b8aA6"), "weETH", 18, &[feed(LINEA_WEETH_ETH_FEED, 18, 18, true)], TokenIcon::None),
    (59144, address!("0xB5beDd42000b71FddE22D3eE8a79Bd49A568fC8F"), "wstETH", 18, &[TokenAnchorSource::Product { sources: &[feed(LINEA_ETH_USD_FEED, 8, 18, false), feed(LINEA_WSTETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (59144, address!("0xa18152629128738a5c081eb226335FEd4B9C95e9"), "LINK", 18, &[feed(LINEA_LINK_ETH_FEED, 18, 18, true)], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (59144, address!("0x0D1E753a25eBda689453309112904807625bEFBe"), "Cake", 18, NO_ANCHORS, TokenIcon::Shared("56-0x0e09fabb73bd3ade0a17ecc321fd13a19e81ce82.png")),
    (59144, address!("0x1789e0043623282D5DCc7F213d703C6D8BAfBB04"), "LINEA", 18, &[TokenAnchorSource::Product { sources: &[feed(LINEA_ETH_USD_FEED, 8, 18, false), feed(LINEA_LINEA_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Katana (747474)
    (747_474, address!("0x80Eede496655FB9047dd39d9f418d5483ED600df"), "frxUSD", 18, &[feed(KATANA_ETH_USD_FEED, 8, 18, false)], TokenIcon::None),
    (747_474, address!("0xecAc9C5F704e954931349Da37F60E39f515c11c1"), "LBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(KATANA_ETH_USD_FEED, 8, 8, false), feed(KATANA_LBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (747_474, address!("0x0913DA6Da4b42f538B445599b46Bb4622342Cf52"), "vbWBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(KATANA_ETH_USD_FEED, 8, 8, false), feed(KATANA_BTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (747_474, address!("0x9893989433e7a383Cb313953e4c2365107dc19a7"), "weETH", 18, &[feed(KATANA_WEETH_ETH_FEED, 8, 18, true)], TokenIcon::None),
    (747_474, address!("0xEE7D8BCFb72bC1880D0Cf19822eB0A2e6577aB62"), "vbETH", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    (747_474, address!("0xc2C447b04e0ED3476DdbDae8E9E39bE7159d27b6"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(KATANA_ETH_USD_FEED, 8, 18, false), feed(KATANA_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (747_474, address!("0x1e5eFCA3D0dB2c6d5C67a4491845c43253eB9e4e"), "MORPHO", 18, &[TokenAnchorSource::Product { sources: &[feed(KATANA_ETH_USD_FEED, 8, 18, false), feed(KATANA_MORPHO_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (747_474, address!("0x7F1f4b4b29f5058fA32CC7a97141b8D7e5ABDC2d"), "KAT", 18, &[TokenAnchorSource::Product { sources: &[feed(KATANA_ETH_USD_FEED, 8, 18, false), feed(KATANA_KAT_USD_FEED, 18, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Rootstock (30)
    (30, address!("0x779Ded0c9e1022225f8E0630b35a9b54bE713736"), "USDT0", 6, NO_ANCHORS, TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (30, address!("0x74c9f2b00581F1B11AA7ff05aa9F608B7389De67"), "USDC.e", 6, NO_ANCHORS, TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (30, address!("0xAf368c91793CB22739386DFCbBb2F1A9e4bCBeBf"), "USDT", 6, NO_ANCHORS, TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (30, address!("0x2F6F07CDcf3588944Bf4C42aC74ff24bF56e7590"), "WETH", 18, NO_ANCHORS, TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (30, address!("0x938D84942f5D924070A6bb82F8e56a5E2b3098A4"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (30, address!("0x2AcC95758f8b5F583470ba265EB685a8F45fC9D5"), "RIF", 18, NO_ANCHORS, TokenIcon::None),
    // Cronos (25)
    (25, address!("0x3D7F2C478aAfdB65542BCB44bCeeC05849999d2D"), "USDC", 6, &[feed(CRONOS_CRO_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (25, address!("0xA6dE01a2d62C6B5f3525d768f34d276652C554c8"), "EURC", 6, NO_ANCHORS, TokenIcon::Shared("1-0x1abaea1f7c830bd89acc67ec4af516284b1bc33c.png")),
    (25, address!("0x66e428c3f67a68878562e79A0234c1F83c208770"), "USDT", 6, &[feed(CRONOS_CRO_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (25, address!("0x062E66477Faf219F25D27dCED647BF57C3107d52"), "WBTC", 8, NO_ANCHORS, TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (25, address!("0x7a7c9db510aB29A2FC362a4c34260BEcB5cE3446"), "CDCETH", 18, NO_ANCHORS, TokenIcon::None),
    (25, address!("0xe44Fd7fCb2b1581822D0c862B68222998a0c299a"), "WETH", 18, NO_ANCHORS, TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (25, address!("0x8c80A01F461f297Df7F9DA3A4f740D7297C8Ac85"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (25, address!("0x2D03bECE6747ADC00E1a131BBA1469C15fD11e03"), "VVS", 18, NO_ANCHORS, TokenIcon::None),
    // MegaETH (4326)
    (4326, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(MEGAETH_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (4326, address!("0xB8CE59FC3717ada4C02eaDF9682A9e934F625ebb"), "USDT0", 6, &[feed(MEGAETH_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (4326, address!("0xB0F70C0bD6FD87dbEb7C10dC692a2a6106817072"), "BTC.b", 8, &[TokenAnchorSource::Product { sources: &[feed(MEGAETH_ETH_USD_FEED, 8, 8, false), feed(MEGAETH_BTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (4326, address!("0x601aC63637933D88285A025C685AC4e9a92a98dA"), "wstETH", 18, NO_ANCHORS, TokenIcon::None),
    (4326, address!("0xee85aEfb15b9489563A6a29891ebe0750AA1A7Ae"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (4326, address!("0x28B7E77f82B25B95953825F1E3eA0E36c1c29861"), "MEGA", 18, &[TokenAnchorSource::Product { sources: &[feed(MEGAETH_ETH_USD_FEED, 8, 18, false), feed(MEGAETH_MEGA_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    // Berachain (80094)
    (80094, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(BERACHAIN_BERA_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (80094, address!("0x779Ded0c9e1022225f8E0630b35a9b54bE713736"), "USDT0", 6, &[feed(BERACHAIN_BERA_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (80094, address!("0x80Eede496655FB9047dd39d9f418d5483ED600df"), "frxUSD", 18, &[feed(BERACHAIN_BERA_USD_FEED, 8, 18, false)], TokenIcon::None),
    (80094, address!("0x0555E30da8f98308EdB960aa94C0Db47230d2B9c"), "WBTC", 8, NO_ANCHORS, TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (80094, address!("0xecAc9C5F704e954931349Da37F60E39f515c11c1"), "LBTC", 8, NO_ANCHORS, TokenIcon::None),
    (80094, address!("0x7DCC39B4d1C53CB31e1aBc0e358b43987FEF80f7"), "weETH", 18, NO_ANCHORS, TokenIcon::None),
    (80094, address!("0x2F6F07CDcf3588944Bf4C42aC74ff24bF56e7590"), "WETH", 18, NO_ANCHORS, TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (80094, address!("0x71052BAe71C25C78E37fD12E5ff1101A71d9018F"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (80094, address!("0xFf9c599D51C407A45D631c6e89cB047Efb88AeF6"), "PENDLE", 18, NO_ANCHORS, TokenIcon::None),
    // World Chain (480)
    (480, address!("0x79A02482A880bCE3F13e09Da970dC34db4CD24d1"), "USDC", 6, &[feed(WORLDCHAIN_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (480, address!("0x1C60ba0A0eD1019e8Eb035E6daF4155A5cE2380B"), "EURC", 6, NO_ANCHORS, TokenIcon::Shared("1-0x1abaea1f7c830bd89acc67ec4af516284b1bc33c.png")),
    (480, address!("0x102d758f688a4C1C5a80b116bD945d4455460282"), "USDT0", 6, &[feed(WORLDCHAIN_ETH_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (480, address!("0x03C7054BCB39f7b2e5B2c7AcB37583e32D70Cfa3"), "WBTC", 8, NO_ANCHORS, TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (480, address!("0x915b648e994d5f31059B38223b9fbe98ae185473"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (480, address!("0x2cFc85d8E48F8EAB294be644d9E25C3030863003"), "WLD", 18, NO_ANCHORS, TokenIcon::None),
    // Plasma (9745)
    (9745, address!("0x2d661C89D812261039AF9764eceaAee884f5F67F"), "USDC", 6, &[feed(PLASMA_XPL_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (9745, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(PLASMA_XPL_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (9745, address!("0xB8CE59FC3717ada4C02eaDF9682A9e934F625ebb"), "USDT0", 6, &[feed(PLASMA_XPL_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (9745, address!("0xA3D68b74bF0528fdD07263c60d6488749044914b"), "weETH", 18, &[TokenAnchorSource::Product { sources: &[feed(PLASMA_XPL_USD_FEED, 8, 18, false), feed(PLASMA_WEETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (9745, address!("0x9895D81bB462A195b4922ED7De0e3ACD007c32CB"), "WETH", 18, &[TokenAnchorSource::Product { sources: &[feed(PLASMA_XPL_USD_FEED, 8, 18, false), feed(PLASMA_ETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (9745, address!("0x76a443768A5e3B8d1AED0105FC250877841Deb40"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(PLASMA_XPL_USD_FEED, 8, 18, false), feed(PLASMA_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (9745, address!("0x6100E367285b01F48D07953803A2d8dCA5D19873"), "WXPL", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    // Etherlink (42793)
    (42793, address!("0x796Ea11Fa2dD751eD01b53C372fFDB4AAa8f00F9"), "USDC", 6, &[feed(ETHERLINK_XTZ_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (42793, address!("0xecAc9C5F704e954931349Da37F60E39f515c11c1"), "LBTC", 8, NO_ANCHORS, TokenIcon::None),
    (42793, address!("0xbFc94CD2B1E55999Cfc7347a9313e88702B83d0F"), "WBTC", 8, NO_ANCHORS, TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (42793, address!("0xfc24f770F94edBca6D6f885E12d4317320BcB401"), "WETH", 18, NO_ANCHORS, TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (42793, address!("0x8ce7618E8f8E514d13889283F58FF03B794e6CC3"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (42793, address!("0x004A476B5B76738E34c86C7144554B9d34402F13"), "CRV", 18, NO_ANCHORS, TokenIcon::None),
    (42793, address!("0xc9B53AB2679f573e480d01e0f49e2B5CFB7a3EAb"), "WXTZ", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    // Stable (988)
    (988, address!("0x779Ded0c9e1022225f8E0630b35a9b54bE713736"), "USDT0", 6, &[TokenAnchorSource::Fixed { token_fee_per_unit_gas: uint!(1_000_000_U256) }], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (988, address!("0x80Eede496655FB9047dd39d9f418d5483ED600df"), "frxUSD", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    (988, address!("0x8a2B28364102Bea189D99A475C494330Ef2bDD0B"), "USDC.e", 6, &[TokenAnchorSource::Fixed { token_fee_per_unit_gas: uint!(1_000_000_U256) }], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (988, address!("0xecAc9C5F704e954931349Da37F60E39f515c11c1"), "LBTC", 8, NO_ANCHORS, TokenIcon::None),
    (988, address!("0xB0F70C0bD6FD87dbEb7C10dC692a2a6106817072"), "BTC.b", 8, NO_ANCHORS, TokenIcon::None),
    (988, address!("0x783129E4d7bA0Af0C896c239E57C06DF379aAE8c"), "WETH", 18, NO_ANCHORS, TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (988, address!("0x985FB0821Eef0056ec26DD8b33dC61b9415B7F4b"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (988, address!("0x0000000000000000000000000000000000001003"), "STABLE", 18, NO_ANCHORS, TokenIcon::None),
    // Monad (143)
    (143, address!("0x754704Bc059F8C67012fEd69BC8A327a5aafb603"), "USDC", 6, &[feed(MONAD_MON_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (143, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(MONAD_MON_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (143, address!("0x111111d2bf19e43C34263401e0CAd979eD1cdb61"), "USD1", 6, &[feed(MONAD_MON_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0x8d0d000ee44948fc98c9b98a4fa4921476f08b0d.png")),
    (143, address!("0x0555E30da8f98308EdB960aa94C0Db47230d2B9c"), "WBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(MONAD_MON_USD_FEED, 8, 8, false), feed(MONAD_WBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::Shared("1-0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png")),
    (143, address!("0xd18B7EC58Cdf4876f6AFebd3Ed1730e4Ce10414b"), "cbBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(MONAD_MON_USD_FEED, 8, 8, false), feed(MONAD_CBBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (143, address!("0x10Aeaf63194db8d453d4D85a06E5eFE1dd0b5417"), "wstETH", 18, &[TokenAnchorSource::Product { sources: &[feed(MONAD_MON_USD_FEED, 8, 18, false), feed(MONAD_WSTETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (143, address!("0xA3D68b74bF0528fdD07263c60d6488749044914b"), "weETH", 18, &[TokenAnchorSource::Product { sources: &[feed(MONAD_MON_USD_FEED, 8, 18, false), feed(MONAD_WEETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::None),
    (143, address!("0xEE8c0E9f1BFFb4Eb878d8f15f368A02a35481242"), "WETH", 18, &[TokenAnchorSource::Product { sources: &[feed(MONAD_MON_USD_FEED, 8, 18, false), feed(MONAD_ETH_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    (143, address!("0x76f257B1DDA5cC71bee4eF637Fbdde4C801310A9"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(MONAD_MON_USD_FEED, 8, 18, false), feed(MONAD_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (143, address!("0x5E49E1f85813F2B65858860A3FA231b4186f2e0E"), "PENDLE", 18, NO_ANCHORS, TokenIcon::None),
    (143, address!("0x3bd359C1119dA7Da1D913D1C4D2B7c461115433A"), "WMON", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    // Robinhood Chain (4663)
    (4663, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(ROBINHOOD_ETH_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (4663, address!("0xCEC185eB182c47d1bA1EFc84e6959e18cd620Be4"), "cbBTC", 8, &[TokenAnchorSource::Product { sources: &[feed(ROBINHOOD_ETH_USD_FEED, 8, 8, false), feed(ROBINHOOD_CBBTC_USD_FEED, 8, 8, true)], scale_decimals: 8 }], TokenIcon::None),
    (4663, address!("0x492641F648a4986844848E0beFE66D14817bCE34"), "LINK", 18, &[TokenAnchorSource::Product { sources: &[feed(ROBINHOOD_ETH_USD_FEED, 8, 18, false), feed(ROBINHOOD_LINK_USD_FEED, 8, 18, true)], scale_decimals: 18 }], TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (4663, address!("0x5E49E1f85813F2B65858860A3FA231b4186f2e0E"), "PENDLE", 18, NO_ANCHORS, TokenIcon::None),
    // Arc (5042)
    (5042, address!("0x3600000000000000000000000000000000000000"), "USDC", 6, &[TokenAnchorSource::Fixed { token_fee_per_unit_gas: uint!(1_000_000_U256) }], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (5042, address!("0xbEf5f6d51CB62b58e6A8f77868681825C6fe21c1"), "EURC", 6, &[feed(ARC_EURC_USD_FEED, 8, 6, true)], TokenIcon::Shared("1-0x1abaea1f7c830bd89acc67ec4af516284b1bc33c.png")),
    (5042, address!("0x128cC466B61f542da60c70e3aA11c10e19B84EDB"), "WETH", 18, &[feed(ARC_ETH_USD_FEED, 8, 18, true)], TokenIcon::Shared("1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")),
    // Pharos (1672)
    (1672, address!("0xC879C018dB60520F4355C26eD1a6D572cdAC1815"), "USDC", 6, &[feed(PHAROS_PROS_USD_FEED, 18, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (1672, address!("0x51e2A24742Db77604B881d6781Ee16B5b8fcBE29"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (1672, address!("0x52C48d4213107b20bC583832b0d951FB9CA8F0B0"), "WPROS", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    // HyperEVM (999)
    (999, address!("0xb88339CB7199b77E23DB6E890353E22632Ba630f"), "USDC", 6, &[feed(HYPEREVM_HYPE_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")),
    (999, address!("0x5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"), "USDe", 18, &[feed(HYPEREVM_HYPE_USD_FEED, 8, 18, false)], TokenIcon::Shared("1-0x4c9edd5852cd905f086c759e8383e09bff1e68b3.png")),
    (999, address!("0xB8CE59FC3717ada4C02eaDF9682A9e934F625ebb"), "USDT0", 6, &[feed(HYPEREVM_HYPE_USD_FEED, 8, 6, false)], TokenIcon::Shared("1-0xdac17f958d2ee523a2206206994597c13d831ec7.png")),
    (999, address!("0xA3D68b74bF0528fdD07263c60d6488749044914b"), "weETH", 18, NO_ANCHORS, TokenIcon::None),
    (999, address!("0x1AC2EE68b8d038C982C1E1f73F596927dd70De59"), "LINK", 18, NO_ANCHORS, TokenIcon::Shared("1-0x514910771af9ca656af840dff83e8264ecf986ca.png")),
    (999, address!("0xD6Eb81136884713E843936843E286FD2a85A205A"), "PENDLE", 18, NO_ANCHORS, TokenIcon::None),
    (999, address!("0x5555555555555555555555555555555555555555"), "WHYPE", 18, WRAPPED_NATIVE_ANCHOR, TokenIcon::None),
    (999, address!("0x9b498C3c8A0b8CD8BA1D9851d40D186F1872b44E"), "PURR", 18, NO_ANCHORS, TokenIcon::None),
];

#[must_use]
pub fn lookup_token(chain_id: u64, addr: &Address) -> Option<TokenInfo> {
    TOKENS
        .iter()
        .find(|(c, a, _, _, _, _)| *c == chain_id && a == addr)
        .map(|(_, _, symbol, decimals, anchor_sources, _)| TokenInfo {
            symbol,
            decimals: *decimals,
            anchor_sources,
        })
}

pub fn token_anchor_entries() -> impl Iterator<Item = TokenAnchorInfo> {
    TOKENS
        .iter()
        .filter(|(_, _, _, _, anchor_sources, _)| !anchor_sources.is_empty())
        .map(
            |(chain_id, token, _, _, anchor_sources, _)| TokenAnchorInfo {
                chain_id: *chain_id,
                token: *token,
                anchor_sources,
            },
        )
}

pub fn known_tokens_for_chain(chain_id: u64) -> impl Iterator<Item = KnownTokenInfo> {
    TOKENS
        .iter()
        .filter(move |(token_chain_id, _, _, _, _, _)| *token_chain_id == chain_id)
        .map(
            |(chain_id, token, symbol, decimals, anchor_sources, _)| KnownTokenInfo {
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
    let (_, _, _, _, _, icon) = TOKENS
        .iter()
        .find(|(c, a, _, _, _, _)| *c == chain_id && a == addr)?;

    match icon {
        TokenIcon::Own => {
            let ext = if (chain_id == 1
                && *addr == address!("0x085780639CC2cACd35E474e71f4d000e2405d8f6"))
                || (chain_id == 42161
                    && *addr == address!("0x4D15a3A2286D883AF0AA1B3f21367843FAc63E07"))
            {
                "svg"
            } else {
                "png"
            };

            Some(format!("{chain_id}-{addr:#x}.{ext}"))
        }
        TokenIcon::Shared(file) => Some((*file).to_owned()),
        TokenIcon::None => None,
    }
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
pub fn native_usd_micro_value(
    amount: U256,
    native_usd_micro_rate: U256,
    native_decimals: u8,
) -> Option<U256> {
    let scale = U256::from(10).checked_pow(U256::from(native_decimals))?;
    token_usd_micro_value(amount, scale, native_usd_micro_rate)
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
            native_usd_micro_value(uint!(2_000_000_000_000_000_000_U256), native_usd, 18),
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
        // Sepolia (11155111) isn't in the registry.
        assert!(lookup_token(11_155_111, &weth).is_none());
        assert_eq!(token_icon_path(11_155_111, &weth), None);
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
    fn every_token_row_resolves_its_vendored_icon() {
        for (chain_id, token, symbol, _, _, icon) in TOKENS {
            let path = token_icon_path(*chain_id, token);
            match icon {
                TokenIcon::Own | TokenIcon::Shared(_) => {
                    let path = path.unwrap_or_else(|| {
                        panic!("{symbol} on chain {chain_id} should resolve an icon path")
                    });
                    assert!(
                        path.exists(),
                        "{symbol} on chain {chain_id} is missing {}",
                        path.display()
                    );
                }
                TokenIcon::None => assert_eq!(
                    path, None,
                    "{symbol} on chain {chain_id} should not resolve an icon path"
                ),
            }
        }
    }

    #[test]
    fn token_rows_are_unique_per_chain_and_address() {
        let mut seen = std::collections::HashSet::new();
        for (chain_id, token, symbol, _, _, _) in TOKENS {
            // `{token:#x}` is lower-case hex, so this compares addresses case-insensitively.
            assert!(
                seen.insert((*chain_id, format!("{token:#x}"))),
                "{symbol} duplicates an existing row on chain {chain_id}"
            );
        }
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
        assert_eq!(chain_name(8453), Some("Base"));
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
        assert_eq!(chain_icon_path(0), None);
        for chain_id in crate::chains::built_in_chain_ids() {
            assert!(
                chain_icon_path(chain_id).is_some_and(|path| path.exists()),
                "built-in chain {chain_id} ships an icon file"
            );
        }
    }
}
