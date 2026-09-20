//! Volatile chain editor drafts shared by native and browser presentation.
//! Numeric input stays textual until the desktop validates a commit. Never persist these drafts
//! in browser storage or include them in general wallet snapshots: endpoints can contain credentials.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChainField {
    Name,
    NativeName,
    NativeSymbol,
    NativeDecimals,
    RpcEndpoints,
    ExplorerUrls,
    WrappedNativeToken,
    MulticallContract,
    FinalityDepth,
    GasLimitBuffer,
    GasPriceBufferNumerator,
    GasPriceBufferDenominator,
    RailgunContract,
    RelayAdaptContract,
    RelayAdapt7702Contract,
    CoinbasePayer,
    DeploymentBlock,
    V2StartBlock,
    LegacyShieldBlock,
    ArchiveUntilBlock,
    ArchiveRpcUrl,
    QuickSyncEndpoint,
    QuickSyncIndexedWalletBlockRange,
    BlockRange,
    PollIntervalSecs,
    IndexedWalletBlockRange,
    SponsoredBundleRelays,
}

impl ChainField {
    pub const ALL: &[Self] = &[
        Self::Name,
        Self::NativeName,
        Self::NativeSymbol,
        Self::NativeDecimals,
        Self::RpcEndpoints,
        Self::ExplorerUrls,
        Self::WrappedNativeToken,
        Self::MulticallContract,
        Self::FinalityDepth,
        Self::GasLimitBuffer,
        Self::GasPriceBufferNumerator,
        Self::GasPriceBufferDenominator,
        Self::RailgunContract,
        Self::RelayAdaptContract,
        Self::RelayAdapt7702Contract,
        Self::CoinbasePayer,
        Self::DeploymentBlock,
        Self::V2StartBlock,
        Self::LegacyShieldBlock,
        Self::ArchiveUntilBlock,
        Self::ArchiveRpcUrl,
        Self::QuickSyncEndpoint,
        Self::QuickSyncIndexedWalletBlockRange,
        Self::BlockRange,
        Self::PollIntervalSecs,
        Self::IndexedWalletBlockRange,
        Self::SponsoredBundleRelays,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Name => "Chain name",
            Self::NativeName => "Native currency name",
            Self::NativeSymbol => "Native currency symbol",
            Self::NativeDecimals => "Native currency decimals",
            Self::RpcEndpoints => "RPC endpoints",
            Self::ExplorerUrls => "Block explorers",
            Self::WrappedNativeToken => "Wrapped native token",
            Self::MulticallContract => "Multicall contract",
            Self::FinalityDepth => "Finality depth",
            Self::GasLimitBuffer => "Gas limit buffer",
            Self::GasPriceBufferNumerator => "Gas price buffer numerator",
            Self::GasPriceBufferDenominator => "Gas price buffer denominator",
            Self::RailgunContract => "Railgun contract",
            Self::RelayAdaptContract => "Relay adapter",
            Self::RelayAdapt7702Contract => "Relay adapter 7702",
            Self::CoinbasePayer => "Coinbase payer",
            Self::DeploymentBlock => "Deployment block",
            Self::V2StartBlock => "V2 start block",
            Self::LegacyShieldBlock => "Legacy Shield block",
            Self::ArchiveUntilBlock => "Archive boundary block",
            Self::ArchiveRpcUrl => "Archive RPC endpoint",
            Self::QuickSyncEndpoint => "Quick-sync endpoint",
            Self::QuickSyncIndexedWalletBlockRange => "Quick-sync indexed block range",
            Self::BlockRange => "Scan block range",
            Self::PollIntervalSecs => "Poll interval (seconds)",
            Self::IndexedWalletBlockRange => "Indexed wallet block range",
            Self::SponsoredBundleRelays => "Sponsored bundle relays",
        }
    }

    #[must_use]
    pub const fn is_railgun(self) -> bool {
        matches!(
            self,
            Self::RailgunContract
                | Self::RelayAdaptContract
                | Self::RelayAdapt7702Contract
                | Self::CoinbasePayer
                | Self::DeploymentBlock
                | Self::V2StartBlock
                | Self::LegacyShieldBlock
                | Self::ArchiveUntilBlock
                | Self::ArchiveRpcUrl
                | Self::QuickSyncEndpoint
                | Self::QuickSyncIndexedWalletBlockRange
                | Self::BlockRange
                | Self::PollIntervalSecs
                | Self::IndexedWalletBlockRange
                | Self::SponsoredBundleRelays
        )
    }

    /// Newline-joined URL lists. The wire value stays one string; presentation may split it.
    #[must_use]
    pub const fn is_url_list(self) -> bool {
        matches!(
            self,
            Self::RpcEndpoints | Self::ExplorerUrls | Self::SponsoredBundleRelays
        )
    }

    #[must_use]
    pub const fn is_multiline(self) -> bool {
        self.is_url_list()
    }

    /// EVM tuning that inherits global or preset values when left alone.
    #[must_use]
    pub const fn is_advanced_evm(self) -> bool {
        matches!(
            self,
            Self::WrappedNativeToken
                | Self::MulticallContract
                | Self::FinalityDepth
                | Self::GasLimitBuffer
                | Self::GasPriceBufferNumerator
                | Self::GasPriceBufferDenominator
        )
    }

    /// Contract addresses and deployment blocks that can make funds unreachable when changed.
    #[must_use]
    pub const fn is_deployment(self) -> bool {
        matches!(
            self,
            Self::RailgunContract
                | Self::RelayAdaptContract
                | Self::RelayAdapt7702Contract
                | Self::CoinbasePayer
                | Self::DeploymentBlock
                | Self::V2StartBlock
                | Self::LegacyShieldBlock
                | Self::ArchiveUntilBlock
                | Self::ArchiveRpcUrl
        )
    }

    #[must_use]
    pub const fn is_identity(self) -> bool {
        matches!(
            self,
            Self::Name
                | Self::NativeName
                | Self::NativeSymbol
                | Self::NativeDecimals
                | Self::ExplorerUrls
        )
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct ChainDraft {
    /// Decimal u64, kept as a string across JavaScript and JSON.
    pub chain_id: String,
    pub built_in: bool,
    pub enabled: bool,
    pub fields: BTreeMap<ChainField, String>,
    pub quick_sync_enabled: bool,
    /// Distinguishes preset relays from an explicitly empty relay list.
    pub use_default_relays: bool,
    /// Inherited values for inheritable fields, for display only. The host owns resolution.
    #[serde(default)]
    pub defaults: BTreeMap<ChainField, String>,
}

impl ChainDraft {
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: true,
            quick_sync_enabled: true,
            use_default_relays: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn value(&self, field: ChainField) -> &str {
        self.fields.get(&field).map_or("", String::as_str)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct ChainSummary {
    pub chain_id: String,
    pub name: String,
    pub enabled: bool,
    pub built_in: bool,
    /// Built-in chain whose saved overrides differ from the preset, ignoring `enabled`.
    #[serde(default)]
    pub modified: bool,
    /// Effective RPC endpoint count. Never carries the endpoint URLs themselves.
    #[serde(default)]
    pub rpc_endpoints: usize,
}

/// Explicit editor intent. The native owner supplies revision/authority admission separately.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ChainEditorCommand {
    List,
    Inspect { chain_id: String },
    Save { draft: ChainDraft, existing: bool },
    Remove { chain_id: String },
    Reset { chain_id: String },
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct ChainEditorSnapshot {
    pub revision: String,
    pub chains: Vec<ChainSummary>,
    pub draft: Option<ChainDraft>,
    pub restart_required: bool,
}
