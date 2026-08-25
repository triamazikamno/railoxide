use super::{
    BroadcasterFeePolicy, DEFAULT_PUBLIC_BROADCASTER_RESPONSE_TIMEOUT_SECS, Deserialize,
    LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME, MAX_INTERVAL_SECS, OFFICIAL_POI_ARTIFACT_GATEWAYS,
    OFFICIAL_POI_ARTIFACT_IPNS_NAME, OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY,
    PUBLIC_BROADCASTER_REPUBLISH_INTERVAL, PoiArtifactManifestSource, PoiArtifactSourceConfig,
    SensitiveUrl, Serialize, Url, parse_fixed_hex_32, validate_optional_range, validate_range,
    validate_url_scheme,
};
use cid::Cid;
use libp2p::PeerId;

const LIBP2P_KEY_CODEC: u64 = 0x72;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PoiReadSourceSetting {
    #[default]
    IndexedArtifacts,
    PoiProxy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PoiSettings {
    pub read_source: PoiReadSourceSetting,
    pub artifact: PoiArtifactSettings,
    pub proxy: PoiProxySettings,
}

impl Default for PoiSettings {
    fn default() -> Self {
        Self {
            read_source: PoiReadSourceSetting::IndexedArtifacts,
            artifact: PoiArtifactSettings::official_preset(),
            proxy: PoiProxySettings::default(),
        }
    }
}

impl PoiSettings {
    pub fn reset_artifact_to_official_preset(&mut self) {
        self.artifact = PoiArtifactSettings::official_preset();
    }

    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        self.artifact.validate_gateway_urls(errors);
        match self.read_source {
            PoiReadSourceSetting::IndexedArtifacts => {
                self.artifact.validate_required_source(errors);
            }
            PoiReadSourceSetting::PoiProxy => {}
        }
        self.proxy.validate(errors);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PoiArtifactSettings {
    pub publisher_pubkey: String,
    pub manifest_source: PoiArtifactManifestSourceSetting,
    pub gateway_urls: Vec<String>,
    pub max_manifest_age_secs: Option<u64>,
}

impl Default for PoiArtifactSettings {
    fn default() -> Self {
        Self::official_preset()
    }
}

impl PoiArtifactSettings {
    #[must_use]
    pub fn official_preset() -> Self {
        Self {
            publisher_pubkey: OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY.to_string(),
            manifest_source: PoiArtifactManifestSourceSetting::IpnsName(
                OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string(),
            ),
            gateway_urls: OFFICIAL_POI_ARTIFACT_GATEWAYS
                .iter()
                .map(ToString::to_string)
                .collect(),
            max_manifest_age_secs: None,
        }
    }

    pub(super) fn migrate_legacy_official_identity(&mut self) -> bool {
        if self.publisher_pubkey != OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY
            || self.manifest_source
                != PoiArtifactManifestSourceSetting::IpnsName(
                    LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string(),
                )
        {
            return false;
        }

        self.manifest_source =
            PoiArtifactManifestSourceSetting::IpnsName(OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string());
        true
    }

    pub(super) fn source_config(&self) -> PoiArtifactSourceConfig {
        PoiArtifactSourceConfig {
            trusted_publisher_pubkey: parse_fixed_hex_32(&self.publisher_pubkey)
                .expect("validated POI publisher public key"),
            manifest_source: self.manifest_source.to_runtime(),
            gateway_urls: self
                .gateway_urls
                .iter()
                .map(|gateway| {
                    SensitiveUrl::from(Url::parse(gateway).expect("validated POI gateway URL"))
                })
                .collect(),
            gateway_pool: None,
            max_manifest_age: self
                .max_manifest_age_secs
                .map(std::time::Duration::from_secs),
        }
    }

    pub(super) fn validate_required_source(&self, errors: &mut Vec<String>) {
        if self.publisher_pubkey.trim().is_empty() {
            errors.push("poi.artifact.publisher_pubkey is required".to_string());
        } else if parse_fixed_hex_32(&self.publisher_pubkey).is_err() {
            errors.push("poi.artifact.publisher_pubkey must be a 32-byte hex value".to_string());
        }
        match &self.manifest_source {
            PoiArtifactManifestSourceSetting::IpnsName(name) => {
                let valid = Cid::try_from(name.trim()).is_ok_and(|cid| {
                    cid.codec() == LIBP2P_KEY_CODEC
                        && PeerId::from_bytes(&cid.hash().to_bytes()).is_ok()
                });
                if !valid {
                    errors
                        .push("poi.artifact.manifest_source must be a valid IPNS name".to_string());
                }
            }
            PoiArtifactManifestSourceSetting::Url(_) | PoiArtifactManifestSourceSetting::Cid(_) => {
                errors.push("poi.artifact.manifest_source must use an IPNS name".to_string());
            }
        }
        validate_optional_range(
            "poi.artifact.max_manifest_age_secs",
            self.max_manifest_age_secs,
            1,
            MAX_INTERVAL_SECS * 365,
            errors,
        );
    }

    pub(super) fn validate_gateway_urls(&self, errors: &mut Vec<String>) {
        if self.gateway_urls.is_empty() {
            errors.push("poi.artifact.gateway_urls must contain at least one gateway".to_string());
        }
        for (index, gateway) in self.gateway_urls.iter().enumerate() {
            validate_url_scheme(
                &format!("poi.artifact.gateway_urls[{index}]"),
                gateway,
                &["http", "https"],
                errors,
            );
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum PoiArtifactManifestSourceSetting {
    Url(String),
    Cid(String),
    IpnsName(String),
}

impl Default for PoiArtifactManifestSourceSetting {
    fn default() -> Self {
        Self::IpnsName(OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string())
    }
}

impl PoiArtifactManifestSourceSetting {
    pub(super) fn to_runtime(&self) -> PoiArtifactManifestSource {
        match self {
            Self::Url(url) => PoiArtifactManifestSource::Url(
                Url::parse(url)
                    .expect("validated POI artifact manifest URL")
                    .into(),
            ),
            Self::Cid(cid) => PoiArtifactManifestSource::Cid(cid.clone()),
            Self::IpnsName(name) => PoiArtifactManifestSource::IpnsName(name.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PoiProxySettings {
    pub rpc_url: String,
}

impl Default for PoiProxySettings {
    fn default() -> Self {
        Self {
            rpc_url: poi::poi::DEFAULT_WALLET_POI_RPC_URL.to_string(),
        }
    }
}

impl PoiProxySettings {
    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        validate_url_scheme(
            "poi.proxy.rpc_url",
            &self.rpc_url,
            &["http", "https"],
            errors,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PublicBroadcasterSettings {
    pub min_anchor_bps: u64,
    pub max_anchor_bps: u64,
    pub allow_suspicious_broadcasters_by_default: bool,
    pub response_timeout_secs: u64,
    pub republish_interval_secs: u64,
}

impl Default for PublicBroadcasterSettings {
    fn default() -> Self {
        let policy = BroadcasterFeePolicy::default();
        Self {
            min_anchor_bps: policy.min_anchor_bps,
            max_anchor_bps: policy.max_anchor_bps,
            allow_suspicious_broadcasters_by_default: policy.allow_suspicious_broadcasters,
            response_timeout_secs: DEFAULT_PUBLIC_BROADCASTER_RESPONSE_TIMEOUT_SECS,
            republish_interval_secs: PUBLIC_BROADCASTER_REPUBLISH_INTERVAL.as_secs(),
        }
    }
}

impl PublicBroadcasterSettings {
    #[must_use]
    pub const fn fee_policy(&self) -> BroadcasterFeePolicy {
        BroadcasterFeePolicy {
            min_anchor_bps: self.min_anchor_bps,
            max_anchor_bps: self.max_anchor_bps,
            allow_suspicious_broadcasters: self.allow_suspicious_broadcasters_by_default,
        }
    }

    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        if self.min_anchor_bps > self.max_anchor_bps {
            errors.push(
                "broadcaster.min_anchor_bps must be less than or equal to max_anchor_bps"
                    .to_string(),
            );
        }
        validate_range(
            "broadcaster.response_timeout_secs",
            self.response_timeout_secs,
            1,
            MAX_INTERVAL_SECS,
            errors,
        );
        validate_range(
            "broadcaster.republish_interval_secs",
            self.republish_interval_secs,
            1,
            MAX_INTERVAL_SECS,
            errors,
        );
    }
}
