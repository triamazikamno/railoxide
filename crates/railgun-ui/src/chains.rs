use std::path::PathBuf;
use std::sync::LazyLock;

static CHAIN_ICON_DIR: LazyLock<PathBuf> =
    LazyLock::new(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/chains"));

pub const DEFAULT_CHAINS: &[u64] = &[1, 56, 137, 42161];

/// Built-in presets without a Railgun deployment. Public EVM operations only.
pub const PUBLIC_CHAINS: &[u64] = &[
    5042, 9745, 5000, 42793, 988, 143, 4663, 146, 130, 57073, 8453, 252, 10, 43114, 100, 81457,
    59144, 747_474, 30, 25, 4326, 80094, 4217, 480, 1672, 999,
];

/// True for any chain the app ships a preset for, with or without Railgun.
#[must_use]
pub fn is_built_in_chain(chain_id: u64) -> bool {
    DEFAULT_CHAINS.contains(&chain_id) || PUBLIC_CHAINS.contains(&chain_id)
}

/// Built-in chain ids, Railgun deployments first.
pub fn built_in_chain_ids() -> impl Iterator<Item = u64> {
    DEFAULT_CHAINS
        .iter()
        .copied()
        .chain(PUBLIC_CHAINS.iter().copied())
}

const fn chain_icon_file(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("ethereum.svg"),
        56 => Some("bsc.svg"),
        137 => Some("polygon.svg"),
        42161 => Some("arbitrum.svg"),
        5042 => Some("arc.svg"),
        9745 => Some("plasma.svg"),
        5000 => Some("mantle.svg"),
        42793 => Some("etherlink.svg"),
        988 => Some("stable.svg"),
        143 => Some("monad.svg"),
        4663 => Some("robinhood.svg"),
        146 => Some("sonic.svg"),
        130 => Some("unichain.svg"),
        57073 => Some("ink.svg"),
        8453 => Some("base.svg"),
        252 => Some("fraxtal.svg"),
        10 => Some("optimism.svg"),
        43114 => Some("avax.svg"),
        100 => Some("gnosis.svg"),
        81457 => Some("blast.svg"),
        59144 => Some("linea.svg"),
        747_474 => Some("katana.svg"),
        30 => Some("rootstock.svg"),
        25 => Some("cronos.svg"),
        4326 => Some("megaeth.svg"),
        80094 => Some("berachain.svg"),
        4217 => Some("tempo.svg"),
        480 => Some("worldchain.svg"),
        1672 => Some("pharosmainnet.png"),
        999 => Some("hyperevm.svg"),
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
        5042 => Some("Arc"),
        9745 => Some("Plasma"),
        5000 => Some("Mantle"),
        42793 => Some("Etherlink"),
        988 => Some("Stable"),
        143 => Some("Monad"),
        4663 => Some("Robinhood Chain"),
        146 => Some("Sonic"),
        130 => Some("Unichain"),
        57073 => Some("Ink"),
        8453 => Some("Base"),
        252 => Some("Fraxtal"),
        10 => Some("Optimism"),
        43114 => Some("Avalanche"),
        100 => Some("Gnosis"),
        81457 => Some("Blast"),
        59144 => Some("Linea"),
        747_474 => Some("Katana"),
        30 => Some("Rootstock"),
        25 => Some("Cronos"),
        4326 => Some("MegaETH"),
        80094 => Some("Berachain"),
        4217 => Some("Tempo"),
        480 => Some("World Chain"),
        1672 => Some("Pharos"),
        999 => Some("HyperEVM"),
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
        5042 => Some("railgun-ui/chains/arc.svg"),
        9745 => Some("railgun-ui/chains/plasma.svg"),
        5000 => Some("railgun-ui/chains/mantle.svg"),
        42793 => Some("railgun-ui/chains/etherlink.svg"),
        988 => Some("railgun-ui/chains/stable.svg"),
        143 => Some("railgun-ui/chains/monad.svg"),
        4663 => Some("railgun-ui/chains/robinhood.svg"),
        146 => Some("railgun-ui/chains/sonic.svg"),
        130 => Some("railgun-ui/chains/unichain.svg"),
        57073 => Some("railgun-ui/chains/ink.svg"),
        8453 => Some("railgun-ui/chains/base.svg"),
        252 => Some("railgun-ui/chains/fraxtal.svg"),
        10 => Some("railgun-ui/chains/optimism.svg"),
        43114 => Some("railgun-ui/chains/avax.svg"),
        100 => Some("railgun-ui/chains/gnosis.svg"),
        81457 => Some("railgun-ui/chains/blast.svg"),
        59144 => Some("railgun-ui/chains/linea.svg"),
        747_474 => Some("railgun-ui/chains/katana.svg"),
        30 => Some("railgun-ui/chains/rootstock.svg"),
        25 => Some("railgun-ui/chains/cronos.svg"),
        4326 => Some("railgun-ui/chains/megaeth.svg"),
        80094 => Some("railgun-ui/chains/berachain.svg"),
        4217 => Some("railgun-ui/chains/tempo.svg"),
        480 => Some("railgun-ui/chains/worldchain.svg"),
        1672 => Some("railgun-ui/chains/pharosmainnet.png"),
        999 => Some("railgun-ui/chains/hyperevm.svg"),
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
