use std::path::PathBuf;
use std::sync::LazyLock;

static CHAIN_ICON_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/chains"));

pub const DEFAULT_CHAINS: &[u64] = &[1, 56, 137, 42161];

const fn chain_icon_file(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("ethereum.svg"),
        56 => Some("bsc.svg"),
        137 => Some("polygon.svg"),
        42161 => Some("arbitrum.svg"),
        _ => None,
    }
}

/// Human-readable name for a chain id. Returns `None` for any chain outside
/// the default app set so callers can decide whether to fall back to the
/// numeric id.
#[must_use]
pub const fn chain_name(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("Ethereum"),
        56 => Some("BSC"),
        137 => Some("Polygon"),
        42161 => Some("Arbitrum"),
        _ => None,
    }
}

#[must_use]
pub fn chain_icon_path(chain_id: u64) -> Option<PathBuf> {
    chain_icon_file(chain_id).map(|file| CHAIN_ICON_DIR.join(file))
}

#[must_use]
pub const fn chain_icon_asset_path(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("railgun-ui/chains/ethereum.svg"),
        56 => Some("railgun-ui/chains/bsc.svg"),
        137 => Some("railgun-ui/chains/polygon.svg"),
        42161 => Some("railgun-ui/chains/arbitrum.svg"),
        _ => None,
    }
}
/// Configured native-currency metadata, shared by desktop and browser presentation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeCurrency {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

impl NativeCurrency {
    #[must_use]
    pub fn format_amount(&self, amount: alloy::primitives::U256) -> String {
        format!(
            "{} {}",
            crate::format_token_amount(amount, self.decimals),
            self.symbol
        )
    }
}
