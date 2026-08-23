use std::time::Duration;

use super::{
    Deserialize, FixedBytes, MAX_INTERVAL_SECS, Serialize, Url, parse_fixed_hex_32,
    validate_optional_range, validate_range, validate_url_scheme,
};

pub const DEFAULT_INDEXED_ARTIFACT_CONCURRENCY: usize = 6;
pub const DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES: u64 = 64 * 1024 * 1024;
pub const OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY: &str =
    "0x053fc2967addee3cec8d637f6de25401a170faaf16e42585952cb1c50abd85fe";
pub const OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME: &str =
    "k51qzi5uqu5dgbarnyr67fxkcmuyb5aumtsi2p86acs8fxpg3q3r0947ne9726";
/// Frozen released-settings compatibility data; runtime defaults use the POI gateway list.
pub const OFFICIAL_INDEXED_ARTIFACT_GATEWAYS: &[&str] = &[
    "https://ipfs.io",
    "https://dweb.link",
    "https://ipfs.filebase.io",
];

const MAX_INDEXED_ARTIFACT_CONCURRENCY: u64 = 32;
const MAX_INDEXED_ARTIFACT_IN_FLIGHT_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IndexedArtifactSourceModeSetting {
    #[default]
    Disabled,
    Official,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct IndexedArtifactSettings {
    pub source_mode: IndexedArtifactSourceModeSetting,
    pub publisher_pubkey: Option<String>,
    pub manifest_source: Option<IndexedArtifactManifestSourceSetting>,
    /// Frozen legacy gateway list retained for released-settings compatibility.
    /// Do not extend this field; runtime indexed-artifact requests use `poi.artifact.gateway_urls`.
    pub gateway_urls: Vec<String>,
    pub max_manifest_age_secs: Option<u64>,
    pub concurrency: Option<usize>,
    pub max_in_flight_bytes: Option<u64>,
}

impl Default for IndexedArtifactSettings {
    fn default() -> Self {
        Self::official_preset()
    }
}

impl IndexedArtifactSettings {
    #[must_use]
    pub fn official_preset() -> Self {
        Self {
            source_mode: IndexedArtifactSourceModeSetting::Official,
            publisher_pubkey: Some(OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY.to_string()),
            manifest_source: Some(IndexedArtifactManifestSourceSetting::IpnsName(
                OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME.to_string(),
            )),
            gateway_urls: OFFICIAL_INDEXED_ARTIFACT_GATEWAYS
                .iter()
                .map(ToString::to_string)
                .collect(),
            max_manifest_age_secs: None,
            concurrency: None,
            max_in_flight_bytes: None,
        }
    }

    #[must_use]
    pub fn disabled_preset() -> Self {
        Self {
            source_mode: IndexedArtifactSourceModeSetting::Disabled,
            publisher_pubkey: Some(OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY.to_string()),
            manifest_source: Some(IndexedArtifactManifestSourceSetting::IpnsName(
                OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME.to_string(),
            )),
            gateway_urls: OFFICIAL_INDEXED_ARTIFACT_GATEWAYS
                .iter()
                .map(ToString::to_string)
                .collect(),
            max_manifest_age_secs: None,
            concurrency: None,
            max_in_flight_bytes: None,
        }
    }

    pub(super) fn source_config(
        &self,
        gateway_urls: &[String],
    ) -> Option<IndexedArtifactSourceConfig> {
        match self.source_mode {
            IndexedArtifactSourceModeSetting::Disabled => None,
            IndexedArtifactSourceModeSetting::Official => {
                Some(Self::official_source_config(gateway_urls))
            }
            IndexedArtifactSourceModeSetting::Custom => {
                Some(self.custom_source_config(gateway_urls))
            }
        }
    }

    fn official_source_config(gateway_urls: &[String]) -> IndexedArtifactSourceConfig {
        IndexedArtifactSourceConfig {
            trusted_publisher_pubkey: parse_fixed_hex_32(
                OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY,
            )
            .expect("official indexed artifact publisher public key is valid"),
            manifest_source: IndexedArtifactManifestSource::IpnsName(
                OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME.to_string(),
            ),
            gateway_urls: gateway_urls
                .iter()
                .map(|gateway| Url::parse(gateway).expect("validated indexed artifact gateway URL"))
                .collect(),
            max_manifest_age: None,
            concurrency: DEFAULT_INDEXED_ARTIFACT_CONCURRENCY,
            max_in_flight_bytes: DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES,
        }
    }

    fn custom_source_config(&self, gateway_urls: &[String]) -> IndexedArtifactSourceConfig {
        IndexedArtifactSourceConfig {
            trusted_publisher_pubkey: parse_fixed_hex_32(
                self.publisher_pubkey
                    .as_deref()
                    .expect("validated indexed artifact publisher public key"),
            )
            .expect("validated indexed artifact publisher public key"),
            manifest_source: self
                .manifest_source
                .as_ref()
                .expect("validated indexed artifact manifest source")
                .to_runtime(),
            gateway_urls: gateway_urls
                .iter()
                .map(|gateway| Url::parse(gateway).expect("validated indexed artifact gateway URL"))
                .collect(),
            max_manifest_age: self.max_manifest_age_secs.map(Duration::from_secs),
            concurrency: self
                .concurrency
                .unwrap_or(DEFAULT_INDEXED_ARTIFACT_CONCURRENCY),
            max_in_flight_bytes: self
                .max_in_flight_bytes
                .unwrap_or(DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES),
        }
    }

    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        if matches!(self.source_mode, IndexedArtifactSourceModeSetting::Custom) {
            self.validate_custom_source(errors);
        }
    }

    fn validate_custom_source(&self, errors: &mut Vec<String>) {
        self.validate_required_source(errors);
        validate_optional_range(
            "indexed_artifacts.max_manifest_age_secs",
            self.max_manifest_age_secs,
            1,
            MAX_INTERVAL_SECS * 365,
            errors,
        );
        validate_optional_usize_range(
            "indexed_artifacts.concurrency",
            self.concurrency,
            1,
            MAX_INDEXED_ARTIFACT_CONCURRENCY,
            errors,
        );
        validate_optional_range(
            "indexed_artifacts.max_in_flight_bytes",
            self.max_in_flight_bytes,
            1,
            MAX_INDEXED_ARTIFACT_IN_FLIGHT_BYTES,
            errors,
        );
    }

    fn validate_required_source(&self, errors: &mut Vec<String>) {
        match self.publisher_pubkey.as_deref() {
            Some(pubkey) if pubkey.trim().is_empty() => {
                errors.push("indexed_artifacts.publisher_pubkey is required".to_string());
            }
            Some(pubkey) if parse_fixed_hex_32(pubkey).is_err() => errors
                .push("indexed_artifacts.publisher_pubkey must be a 32-byte hex value".to_string()),
            Some(_) => {}
            None => errors.push("indexed_artifacts.publisher_pubkey is required".to_string()),
        }
        match &self.manifest_source {
            Some(source) => source.validate("indexed_artifacts.manifest_source", errors),
            None => errors.push("indexed_artifacts.manifest_source is required".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum IndexedArtifactManifestSourceSetting {
    Url(String),
    Cid(String),
    IpnsName(String),
}

impl IndexedArtifactManifestSourceSetting {
    pub(super) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        match self {
            Self::Url(url) => validate_url_scheme(field, url, &["http", "https"], errors),
            Self::Cid(cid) | Self::IpnsName(cid) => {
                if cid.trim().is_empty() {
                    errors.push(format!("{field} must not be empty"));
                }
            }
        }
    }

    pub(super) fn to_runtime(&self) -> IndexedArtifactManifestSource {
        match self {
            Self::Url(url) => IndexedArtifactManifestSource::Url(
                Url::parse(url).expect("validated indexed artifact manifest URL"),
            ),
            Self::Cid(cid) => IndexedArtifactManifestSource::Cid(cid.trim().to_string()),
            Self::IpnsName(name) => {
                IndexedArtifactManifestSource::IpnsName(name.trim().to_string())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexedArtifactManifestSource {
    Url(Url),
    Cid(String),
    IpnsName(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedArtifactSourceConfig {
    pub trusted_publisher_pubkey: FixedBytes<32>,
    pub manifest_source: IndexedArtifactManifestSource,
    pub gateway_urls: Vec<Url>,
    pub max_manifest_age: Option<Duration>,
    pub concurrency: usize,
    pub max_in_flight_bytes: u64,
}

fn validate_optional_usize_range(
    field: &str,
    value: Option<usize>,
    min: u64,
    max: u64,
    errors: &mut Vec<String>,
) {
    if let Some(value) = value {
        validate_range(field, value as u64, min, max, errors);
    }
}
