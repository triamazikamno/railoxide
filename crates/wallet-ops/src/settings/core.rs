use super::{
    ChainSettings, Deserialize, Error, GasSettings, IndexedArtifactSettings, NetworkSettings,
    PoiProxyFallback, PoiReadSource, PoiReadSourceSetting, PoiSettings, PublicBroadcasterSettings,
    RuntimeSettings, SensitiveUrl, Serialize, TokenSettings, Url, WakuSettings,
    WalletConnectSettings, WalletNetworkMode, fmt,
};

pub const WALLET_SETTINGS_KEY: &str = "wallet-settings";
pub const WALLET_SETTINGS_VERSION: u32 = 6;
pub const WALLET_UI_STATE_KEY: &str = "wallet-ui-state";
pub const WALLET_UI_STATE_VERSION: u32 = 2;
pub const OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY: &str =
    "0x4fa849f01e8983c4393eee6e7482f60d4f9702e2d7917101a0edeb001369d5c5";
pub const LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME: &str =
    "k51qzi5uqu5di629evs7ynhsqiy4uit6qt70tx62roace2ij6jc83uo9jseqit";
pub const OFFICIAL_POI_ARTIFACT_IPNS_NAME: &str =
    "k51qzi5uqu5dkrtrukbpsi4pbdfphcrceqgujgft1fxaa4lfgrdp46f4mk7qdq";
pub const OFFICIAL_POI_ARTIFACT_GATEWAYS: &[&str] = &[
    "https://dweb.link",
    "https://ipfs.filebase.io",
    "https://ipfs.io",
    "https://gateway.pinata.cloud",
];
pub const DEFAULT_WAKU_CLUSTER_ID: u32 = 5;
pub const DEFAULT_WAKU_SHARD_ID: u32 = 1;
pub const DEFAULT_WAKU_MAX_PEERS: usize = 10;
pub const DEFAULT_WAKU_PEER_CONNECTION_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_PUBLIC_BROADCASTER_RESPONSE_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_WAKU_DIRECT_PEER_ID: &str =
    "16Uiu2HAkwhijhoc4UxAJD4fmYgSX91FzSDqehAaxJogYFcyo736a";
pub const DEFAULT_WAKU_DIRECT_PEER_ADDR: &str = "/dns4/baaamooobaaa.mooo.com/tcp/8000/wss";

pub(super) const MAX_FINALITY_DEPTH: u64 = 1_000_000;
pub(super) const MAX_BLOCK_RANGE: u64 = 5_000_000;
pub(super) const MAX_INTERVAL_SECS: u64 = 86_400;
pub(super) const SUPPORTED_PROXY_SCHEMES: &[&str] = &["http", "https", "socks5", "socks5h"];

#[derive(Debug, Error)]
pub enum WalletSettingsError {
    #[error(transparent)]
    Db(#[from] local_db::DbError),
    #[error("encode wallet settings: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode wallet settings: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("unsupported wallet settings version {version}")]
    UnsupportedVersion { version: u32 },
    #[error("wallet settings validation failed: {0}")]
    Validation(#[from] WalletSettingsValidationError),
}

#[derive(Debug, Error)]
pub enum WalletUiStateError {
    #[error(transparent)]
    Db(#[from] local_db::DbError),
    #[error("encode wallet UI state: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode wallet UI state: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("unsupported wallet UI state version {version}")]
    UnsupportedVersion { version: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletSettingsValidationError {
    pub messages: Vec<String>,
}

impl WalletSettingsValidationError {
    #[must_use]
    pub const fn new(messages: Vec<String>) -> Self {
        Self { messages }
    }
}

impl fmt::Display for WalletSettingsValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.messages.join("; "))
    }
}

impl std::error::Error for WalletSettingsValidationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WalletSettings {
    pub version: u32,
    pub network: NetworkSettings,
    pub privacy: PrivacySettings,
    pub chains: ChainSettings,
    pub indexed_artifacts: IndexedArtifactSettings,
    pub poi: PoiSettings,
    pub broadcaster: PublicBroadcasterSettings,
    pub tokens: TokenSettings,
    pub gas: GasSettings,
    pub runtime: RuntimeSettings,
    pub waku: WakuSettings,
    pub walletconnect: WalletConnectSettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacySettings {
    pub mimic_railway_shields_by_default: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RememberedWalletKind {
    #[default]
    Unknown,
    SoftwareProfile,
    HardwareWallet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WalletUiState {
    pub version: u32,
    pub last_wallet_id: Option<String>,
    pub last_chain_id: Option<u64>,
    pub last_wallet_kind: RememberedWalletKind,
}

impl Default for WalletUiState {
    fn default() -> Self {
        Self {
            version: WALLET_UI_STATE_VERSION,
            last_wallet_id: None,
            last_chain_id: None,
            last_wallet_kind: RememberedWalletKind::Unknown,
        }
    }
}

impl Default for WalletSettings {
    fn default() -> Self {
        Self {
            version: WALLET_SETTINGS_VERSION,
            network: NetworkSettings::default(),
            privacy: PrivacySettings::default(),
            chains: ChainSettings::default(),
            indexed_artifacts: IndexedArtifactSettings::default(),
            poi: PoiSettings::default(),
            broadcaster: PublicBroadcasterSettings::default(),
            tokens: TokenSettings::default(),
            gas: GasSettings::default(),
            runtime: RuntimeSettings::default(),
            waku: WakuSettings::default(),
            walletconnect: WalletConnectSettings::default(),
        }
    }
}

impl WalletSettings {
    pub fn validate(&self) -> Result<(), WalletSettingsValidationError> {
        let mut errors = Vec::new();
        self.network.validate(&mut errors);
        self.chains.validate(&mut errors);
        self.indexed_artifacts.validate(&mut errors);
        self.poi.validate(&mut errors);
        self.broadcaster.validate(&mut errors);
        self.tokens.validate(&mut errors);
        self.gas.validate(&mut errors);
        self.runtime.validate(&mut errors);
        self.waku.validate(&mut errors);
        self.walletconnect.validate(&mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(WalletSettingsValidationError::new(errors))
        }
    }

    #[must_use]
    pub fn reset_to_defaults() -> Self {
        Self::default()
    }

    pub fn reset_network(&mut self) {
        self.network = NetworkSettings::default();
    }

    pub fn reset_privacy(&mut self) {
        self.privacy = PrivacySettings::default();
    }

    pub fn reset_chains(&mut self) {
        self.chains = ChainSettings::default();
    }

    pub fn reset_indexed_artifacts(&mut self) {
        self.indexed_artifacts = IndexedArtifactSettings::default();
    }

    pub fn reset_poi(&mut self) {
        self.poi = PoiSettings::default();
    }

    pub fn reset_broadcaster(&mut self) {
        self.broadcaster = PublicBroadcasterSettings::default();
    }

    pub fn reset_tokens(&mut self) {
        self.tokens = TokenSettings::default();
    }

    pub fn reset_gas(&mut self) {
        self.gas = GasSettings::default();
    }

    pub fn reset_runtime(&mut self) {
        self.runtime = RuntimeSettings::default();
    }

    pub fn reset_waku(&mut self) {
        self.waku = WakuSettings::default();
    }

    pub fn reset_walletconnect(&mut self) {
        self.walletconnect = WalletConnectSettings::default();
    }

    #[must_use]
    pub fn wallet_network_mode(&self) -> WalletNetworkMode {
        self.network.mode.into()
    }

    pub fn poi_read_source(&self) -> Result<PoiReadSource, WalletSettingsValidationError> {
        let mut errors = Vec::new();
        self.poi.validate(&mut errors);
        if !errors.is_empty() {
            return Err(WalletSettingsValidationError::new(errors));
        }
        let rpc_url = self.poi_rpc_url()?;
        Ok(match self.poi.read_source {
            PoiReadSourceSetting::PoiProxy => PoiReadSource::PoiProxy { rpc_url },
            PoiReadSourceSetting::IndexedArtifacts => PoiReadSource::IndexedArtifacts {
                artifact_source: self.poi.artifact.source_config(),
                rpc_url,
                wallet_read_fallback: PoiProxyFallback::Disabled,
            },
        })
    }

    pub fn poi_rpc_url(&self) -> Result<SensitiveUrl, WalletSettingsValidationError> {
        let mut errors = Vec::new();
        self.poi.proxy.validate(&mut errors);
        if !errors.is_empty() {
            return Err(WalletSettingsValidationError::new(errors));
        }
        Url::parse(&self.poi.proxy.rpc_url)
            .map(SensitiveUrl::from)
            .map_err(|error| {
                WalletSettingsValidationError::new(vec![format!("invalid POI RPC URL: {error}")])
            })
    }
}
