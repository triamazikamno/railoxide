use std::{collections::BTreeSet, fmt};

use super::{
    Address, BTreeMap, Deserialize, EncryptedRecord, HardwareDerivationDescriptor,
    HardwareDeviceKind, HardwarePublicAccountDescriptor, KEY_LEN, RecordKind, Serialize,
    SpendGrant, U256, VaultError, ViewingKeyData, WalletKeys, Zeroize, Zeroizing, fill,
};
use serde::{Deserializer, Serializer, de};
use sha2::Digest;

use crate::hardware::HardwareTypedDataSigningMode;

#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct WalletViewBundle {
    pub derivation_index: u32,
    pub spending_public_key: [[u8; KEY_LEN]; 2],
    pub viewing_private_key: [u8; KEY_LEN],
    pub viewing_public_key: [u8; KEY_LEN],
    pub nullifying_key: [u8; KEY_LEN],
    pub master_public_key: [u8; KEY_LEN],
}

impl WalletViewBundle {
    #[must_use]
    pub fn from_wallet_keys(derivation_index: u32, wallet: &WalletKeys) -> Self {
        Self {
            derivation_index,
            spending_public_key: wallet.spending_public_key.map(|value| value.to_be_bytes()),
            viewing_private_key: wallet.viewing.viewing_private_key,
            viewing_public_key: wallet.viewing.viewing_public_key,
            nullifying_key: wallet.viewing.nullifying_key.to_be_bytes(),
            master_public_key: wallet.viewing.master_public_key.to_be_bytes(),
        }
    }

    #[must_use]
    pub const fn scan_keys(&self) -> ViewingKeyData {
        ViewingKeyData {
            viewing_private_key: self.viewing_private_key,
            viewing_public_key: self.viewing_public_key,
            nullifying_key: U256::from_be_bytes(self.nullifying_key),
            master_public_key: U256::from_be_bytes(self.master_public_key),
        }
    }

    #[must_use]
    pub const fn spending_public_key(&self) -> [U256; 2] {
        [
            U256::from_be_bytes(self.spending_public_key[0]),
            U256::from_be_bytes(self.spending_public_key[1]),
        ]
    }
}

#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct WalletSpendBundle {
    pub derivation_index: u32,
    pub bip39_language: String,
    pub bip39_entropy: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum WalletStatus {
    #[default]
    Active,
    Inactive,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum WalletSource {
    Generated,
    #[default]
    Imported,
    LedgerDerived,
    TrezorDerived,
}

impl WalletSource {
    #[must_use]
    pub const fn is_hardware_derived(self) -> bool {
        matches!(self, Self::LedgerDerived | Self::TrezorDerived)
    }

    #[must_use]
    pub const fn from_hardware_device_kind(device_kind: HardwareDeviceKind) -> Self {
        match device_kind {
            HardwareDeviceKind::Ledger => Self::LedgerDerived,
            HardwareDeviceKind::Trezor => Self::TrezorDerived,
        }
    }
}

pub const SOFTWARE_CONTEXT_VERSION: u32 = 1;
pub const SOFTWARE_CONTEXT_SEED_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VaultSessionId([u8; 16]);

impl VaultSessionId {
    pub fn random() -> Result<Self, VaultError> {
        let mut id = [0u8; 16];
        fill(&mut id).map_err(|_| VaultError::Random)?;
        Ok(Self(id))
    }

    #[must_use]
    pub const fn from_bytes(id: [u8; 16]) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    #[must_use]
    pub fn as_str(&self) -> String {
        alloy::hex::encode(self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftwareSeedSessionBinding {
    base_profile_uuid: String,
    context_wallet_uuid: String,
    vault_session_id: VaultSessionId,
}

impl SoftwareSeedSessionBinding {
    #[must_use]
    pub fn new(
        base_profile_uuid: impl Into<String>,
        context_wallet_uuid: impl Into<String>,
        vault_session_id: VaultSessionId,
    ) -> Self {
        Self {
            base_profile_uuid: base_profile_uuid.into(),
            context_wallet_uuid: context_wallet_uuid.into(),
            vault_session_id,
        }
    }

    #[must_use]
    pub fn base_profile_uuid(&self) -> &str {
        &self.base_profile_uuid
    }

    #[must_use]
    pub fn context_wallet_uuid(&self) -> &str {
        &self.context_wallet_uuid
    }

    #[must_use]
    pub const fn vault_session_id(&self) -> VaultSessionId {
        self.vault_session_id
    }

    #[must_use]
    pub fn record_id(&self) -> String {
        let vault_session_id = self.vault_session_id.as_str();
        format!(
            "software-context-seed:v1:base:{}:{}:context:{}:{}:vault-session:{}:{}",
            self.base_profile_uuid.len(),
            self.base_profile_uuid,
            self.context_wallet_uuid.len(),
            self.context_wallet_uuid,
            vault_session_id.len(),
            vault_session_id,
        )
    }
}

pub struct ProtectedSoftwareSeedSession {
    encrypted_seed: EncryptedRecord,
    binding: SoftwareSeedSessionBinding,
}

impl ProtectedSoftwareSeedSession {
    pub(super) const fn from_parts(
        binding: SoftwareSeedSessionBinding,
        encrypted_seed: EncryptedRecord,
    ) -> Self {
        Self {
            encrypted_seed,
            binding,
        }
    }

    #[must_use]
    pub const fn binding(&self) -> &SoftwareSeedSessionBinding {
        &self.binding
    }

    #[must_use]
    pub const fn encrypted_record(&self) -> &EncryptedRecord {
        &self.encrypted_seed
    }

    pub fn open(
        &self,
        grant: &mut SpendGrant,
        expected_binding: &SoftwareSeedSessionBinding,
    ) -> Result<Zeroizing<[u8; SOFTWARE_CONTEXT_SEED_LEN]>, VaultError> {
        if &self.binding != expected_binding {
            return Err(VaultError::SoftwareSeedSessionBindingMismatch);
        }
        let spend = grant.take_spend_unlock()?;
        let plaintext = spend.decrypt_record(
            RecordKind::SoftwareContextSeed,
            &self.binding.record_id(),
            &self.encrypted_seed,
        )?;
        if plaintext.len() != SOFTWARE_CONTEXT_SEED_LEN {
            return Err(VaultError::InvalidSoftwareSeedLength);
        }
        let mut seed = [0u8; SOFTWARE_CONTEXT_SEED_LEN];
        seed.copy_from_slice(&plaintext);
        Ok(Zeroizing::new(seed))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WalletSoftwareContextKind {
    Standard,
    Passphrase,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletSoftwareContext {
    pub version: u32,
    pub base_profile_uuid: String,
    pub kind: WalletSoftwareContextKind,
    #[serde(default)]
    pub auto_open_standard_context: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftwareContextSyncIntent {
    CreateNew,
    RecoverExisting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftwareContextChainInput {
    pub chain_type: u8,
    pub chain_id: u64,
    pub contract: String,
    pub deployment_block: u64,
    pub current_safe_head: Option<u64>,
}

pub enum CreateSoftwareContextResult {
    ExistingContext {
        metadata: WalletMetadataBundle,
        protected_seed_session: ProtectedSoftwareSeedSession,
    },
    Created {
        metadata: WalletMetadataBundle,
        public_account: Box<PublicAccountMetadata>,
        chain_metadata: Vec<WalletChainMetadataBundle>,
        protected_seed_session: ProtectedSoftwareSeedSession,
    },
}

impl WalletSoftwareContext {
    #[must_use]
    pub fn standard(base_profile_uuid: impl Into<String>) -> Self {
        Self {
            version: SOFTWARE_CONTEXT_VERSION,
            base_profile_uuid: base_profile_uuid.into(),
            kind: WalletSoftwareContextKind::Standard,
            auto_open_standard_context: false,
        }
    }

    #[must_use]
    pub fn passphrase(base_profile_uuid: impl Into<String>) -> Self {
        Self {
            version: SOFTWARE_CONTEXT_VERSION,
            base_profile_uuid: base_profile_uuid.into(),
            kind: WalletSoftwareContextKind::Passphrase,
            auto_open_standard_context: false,
        }
    }

    pub(super) fn validate_for_wallet(&self, wallet_uuid: &str) -> Result<(), VaultError> {
        if self.version != SOFTWARE_CONTEXT_VERSION || self.base_profile_uuid.is_empty() {
            return Err(VaultError::InvalidWalletMetadata);
        }
        if matches!(self.kind, WalletSoftwareContextKind::Standard)
            && self.base_profile_uuid != wallet_uuid
        {
            return Err(VaultError::InvalidWalletMetadata);
        }
        if matches!(self.kind, WalletSoftwareContextKind::Passphrase)
            && (self.base_profile_uuid == wallet_uuid || self.auto_open_standard_context)
        {
            return Err(VaultError::InvalidWalletMetadata);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletSpendSource {
    Software,
    HardwareDerived(HardwareDerivationDescriptor),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletMetadataBundle {
    pub wallet_uuid: String,
    pub label: String,
    pub derivation_index: u32,
    #[serde(default)]
    pub source: WalletSource,
    #[serde(default)]
    pub status: WalletStatus,
    #[serde(default)]
    pub display_order: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_descriptor: Option<HardwareDerivationDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_account: Option<HardwareRailgunAccountMetadata>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub pending_create_new_chain_ids: BTreeSet<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub software_context: Option<WalletSoftwareContext>,
}

impl WalletMetadataBundle {
    #[must_use]
    pub fn hardware_derivation_descriptor(&self) -> Option<&HardwareDerivationDescriptor> {
        self.hardware_account
            .as_ref()
            .map(|account| &account.descriptor)
            .or(self.hardware_descriptor.as_ref())
    }

    pub(super) fn validate(&self) -> Result<(), VaultError> {
        if self.source.is_hardware_derived()
            || self.hardware_descriptor.is_some()
            || self.hardware_account.is_some()
        {
            if self.software_context.is_some() {
                return Err(VaultError::InvalidWalletMetadata);
            }
            return Ok(());
        }

        let Some(context) = self.software_context.as_ref() else {
            return Err(VaultError::InvalidWalletMetadata);
        };
        context.validate_for_wallet(&self.wallet_uuid)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct HardwareWalletProfile {
    pub device_kind: HardwareDeviceKind,
    pub profile_fingerprint: String,
}

impl HardwareWalletProfile {
    #[must_use]
    pub const fn new(device_kind: HardwareDeviceKind, profile_fingerprint: String) -> Self {
        Self {
            device_kind,
            profile_fingerprint,
        }
    }

    #[must_use]
    pub fn profile_id(&self) -> String {
        hardware_profile_id_for_binding(self.device_kind, &self.profile_fingerprint)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HardwareProfilePassphraseState {
    #[default]
    Unknown,
    NotUsed,
    Used,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrezorPassphraseMode {
    #[default]
    NoPassphrase,
    EnterOnTrezor,
    EnterInApp,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HardwareProfileBindingKind {
    #[default]
    EvmAddressFingerprint,
    NativeRailgunFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct HardwareProfileBinding {
    #[serde(default)]
    pub kind: HardwareProfileBindingKind,
    pub fingerprint: String,
}

impl HardwareProfileBinding {
    #[must_use]
    pub fn evm_address_fingerprint(fingerprint: impl Into<String>) -> Self {
        Self {
            kind: HardwareProfileBindingKind::EvmAddressFingerprint,
            fingerprint: fingerprint.into(),
        }
    }

    #[must_use]
    pub fn from_descriptor(descriptor: &HardwareDerivationDescriptor) -> Self {
        Self::evm_address_fingerprint(descriptor.profile_fingerprint.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareTypedDataSessionCapability {
    pub mode: HardwareTypedDataSigningMode,
    device_kind: HardwareDeviceKind,
    profile_id: Option<String>,
    binding: HardwareProfileBinding,
    public_account_path: Vec<u32>,
    trezor_session_id: Option<Vec<u8>>,
}

impl HardwareTypedDataSessionCapability {
    #[must_use]
    fn new(
        session: &HardwareProfileSession,
        descriptor: &HardwarePublicAccountDescriptor,
        mode: HardwareTypedDataSigningMode,
    ) -> Self {
        Self {
            mode,
            device_kind: session.device_kind,
            profile_id: session.profile_id.clone(),
            binding: session.binding.clone(),
            public_account_path: descriptor.path.clone(),
            trezor_session_id: session.trezor_session_id.clone(),
        }
    }

    #[must_use]
    fn matches(
        &self,
        session: &HardwareProfileSession,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> bool {
        let public_account_path_matches = self.public_account_path == descriptor.path;
        self.device_kind == session.device_kind
            && self.profile_id == session.profile_id
            && self.binding == session.binding
            && public_account_path_matches
            && self.trezor_session_id == session.trezor_session_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareProfileSession {
    pub device_kind: HardwareDeviceKind,
    pub profile_id: Option<String>,
    pub binding: HardwareProfileBinding,
    pub trezor_session_id: Option<Vec<u8>>,
    pub trezor_passphrase_mode: Option<TrezorPassphraseMode>,
    pub typed_data_capability: Option<HardwareTypedDataSessionCapability>,
}

impl HardwareProfileSession {
    #[must_use]
    pub const fn unmatched(
        device_kind: HardwareDeviceKind,
        binding: HardwareProfileBinding,
        trezor_session_id: Option<Vec<u8>>,
    ) -> Self {
        Self {
            device_kind,
            profile_id: None,
            binding,
            trezor_session_id,
            trezor_passphrase_mode: None,
            typed_data_capability: None,
        }
    }

    #[must_use]
    pub fn matched(
        device_kind: HardwareDeviceKind,
        profile_id: impl Into<String>,
        binding: HardwareProfileBinding,
        trezor_session_id: Option<Vec<u8>>,
    ) -> Self {
        Self {
            device_kind,
            profile_id: Some(profile_id.into()),
            binding,
            trezor_session_id,
            trezor_passphrase_mode: None,
            typed_data_capability: None,
        }
    }

    pub fn set_trezor_passphrase_mode(&mut self, mode: TrezorPassphraseMode) {
        self.trezor_passphrase_mode = Some(mode);
        self.clear_typed_data_signing_mode();
    }

    #[must_use]
    pub const fn trezor_passphrase_mode(&self) -> TrezorPassphraseMode {
        match self.trezor_passphrase_mode {
            Some(mode) => mode,
            None => TrezorPassphraseMode::NoPassphrase,
        }
    }

    #[must_use]
    pub const fn uses_trezor_app_passphrase(&self) -> bool {
        matches!(
            (self.device_kind, self.trezor_passphrase_mode()),
            (HardwareDeviceKind::Trezor, TrezorPassphraseMode::EnterInApp)
        )
    }

    #[must_use]
    pub fn wallet_profile(&self) -> Option<HardwareWalletProfile> {
        (self.binding.kind == HardwareProfileBindingKind::EvmAddressFingerprint).then(|| {
            HardwareWalletProfile {
                device_kind: self.device_kind,
                profile_fingerprint: self.binding.fingerprint.clone(),
            }
        })
    }

    #[must_use]
    pub fn matches_profile(&self, profile: &HardwareProfileMetadata) -> bool {
        profile.device_kind == self.device_kind
            && profile
                .bindings
                .iter()
                .any(|binding| binding == &self.binding)
    }

    pub fn verify_descriptor(
        &self,
        descriptor: &HardwareDerivationDescriptor,
    ) -> Result<(), VaultError> {
        descriptor
            .validate()
            .map_err(|_| VaultError::InvalidHardwareWalletDescriptor)?;
        if descriptor.device_kind != self.device_kind {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        }
        if self.binding.kind == HardwareProfileBindingKind::EvmAddressFingerprint
            && descriptor.profile_fingerprint != self.binding.fingerprint
        {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        }
        Ok(())
    }

    pub fn verify_account(
        &self,
        account: &HardwareRailgunAccountMetadata,
    ) -> Result<(), VaultError> {
        let Some(profile_id) = self.profile_id.as_ref() else {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        };
        if account.profile_id != *profile_id
            || account.account_index != account.descriptor.account_index
        {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        }
        if !account.custody_backend.is_supported() {
            return Err(VaultError::UnsupportedHardwareCustodyBackend(
                account.custody_backend.as_str().to_owned(),
            ));
        }
        self.verify_descriptor(&account.descriptor)
    }

    #[must_use]
    pub fn typed_data_signing_mode(
        &self,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> Option<HardwareTypedDataSigningMode> {
        self.typed_data_capability
            .as_ref()
            .filter(|capability| capability.matches(self, descriptor))
            .map(|capability| capability.mode)
    }

    pub fn cache_typed_data_signing_mode(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
        mode: HardwareTypedDataSigningMode,
    ) -> Result<(), VaultError> {
        descriptor
            .validate()
            .map_err(|_| VaultError::InvalidHardwareWalletDescriptor)?;
        if descriptor.device_kind != self.device_kind {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        }
        self.typed_data_capability = Some(HardwareTypedDataSessionCapability::new(
            self, descriptor, mode,
        ));
        Ok(())
    }

    pub fn clear_typed_data_signing_mode(&mut self) {
        self.typed_data_capability = None;
    }

    pub fn replace_trezor_session_id_preserving_typed_data_signing_mode(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
        session_id: Option<Vec<u8>>,
    ) -> Result<(), VaultError> {
        let mode = self.typed_data_signing_mode(descriptor);
        self.trezor_session_id = session_id;
        if let Some(mode) = mode {
            self.cache_typed_data_signing_mode(descriptor, mode)?;
        } else {
            self.clear_typed_data_signing_mode();
        }
        Ok(())
    }

    pub fn downgrade_typed_data_signing_mode_to_hash_fallback(
        &mut self,
        descriptor: &HardwarePublicAccountDescriptor,
    ) -> Result<bool, VaultError> {
        if self.typed_data_signing_mode(descriptor) != Some(HardwareTypedDataSigningMode::ClearSign)
        {
            return Ok(false);
        }
        self.cache_typed_data_signing_mode(
            descriptor,
            HardwareTypedDataSigningMode::Eip712HashFallback,
        )?;
        Ok(true)
    }

    pub fn discard_trezor_session(&mut self) {
        self.trezor_session_id = None;
        self.clear_typed_data_signing_mode();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareProfileMetadata {
    pub profile_id: String,
    pub device_kind: HardwareDeviceKind,
    pub label: String,
    #[serde(default)]
    pub passphrase_used: HardwareProfilePassphraseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_trezor_passphrase_mode: Option<TrezorPassphraseMode>,
    #[serde(default)]
    pub bindings: Vec<HardwareProfileBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_unix_seconds: Option<u64>,
}

impl HardwareProfileMetadata {
    #[must_use]
    pub fn from_binding(
        device_kind: HardwareDeviceKind,
        label: impl Into<String>,
        binding: HardwareProfileBinding,
    ) -> Self {
        Self {
            profile_id: hardware_profile_id_for_binding(device_kind, &binding.fingerprint),
            device_kind,
            label: label.into(),
            passphrase_used: HardwareProfilePassphraseState::Unknown,
            preferred_trezor_passphrase_mode: None,
            bindings: vec![binding],
            last_seen_unix_seconds: None,
        }
    }

    #[must_use]
    pub fn from_descriptor(descriptor: &HardwareDerivationDescriptor) -> Self {
        let label = match descriptor.device_kind {
            HardwareDeviceKind::Ledger => "Ledger hardware profile",
            HardwareDeviceKind::Trezor => "Trezor hardware profile",
        };
        Self::from_binding(
            descriptor.device_kind,
            label,
            HardwareProfileBinding::from_descriptor(descriptor),
        )
    }

    #[must_use]
    pub fn matches_wallet_profile(&self, profile: &HardwareWalletProfile) -> bool {
        self.device_kind == profile.device_kind
            && self.bindings.iter().any(|binding| {
                binding.kind == HardwareProfileBindingKind::EvmAddressFingerprint
                    && binding.fingerprint == profile.profile_fingerprint
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareRailgunAccountIdentity {
    pub spending_public_key: [[u8; KEY_LEN]; 2],
    pub viewing_public_key: [u8; KEY_LEN],
}

impl HardwareRailgunAccountIdentity {
    #[must_use]
    pub fn from_wallet_keys(wallet: &WalletKeys) -> Self {
        Self {
            spending_public_key: wallet.spending_public_key.map(|value| value.to_be_bytes()),
            viewing_public_key: wallet.viewing.viewing_public_key,
        }
    }

    #[must_use]
    pub const fn from_view_bundle(bundle: &WalletViewBundle) -> Self {
        Self {
            spending_public_key: bundle.spending_public_key,
            viewing_public_key: bundle.viewing_public_key,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HardwareRailgunAccountCustodyBackend {
    SyntheticSoftwareV1,
    NativeRailgunV1,
    Unsupported(String),
}

impl HardwareRailgunAccountCustodyBackend {
    const SYNTHETIC_SOFTWARE_V1: &'static str = "synthetic_software_v1";
    const NATIVE_RAILGUN_V1: &'static str = "native_railgun_v1";

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::SyntheticSoftwareV1 => Self::SYNTHETIC_SOFTWARE_V1,
            Self::NativeRailgunV1 => Self::NATIVE_RAILGUN_V1,
            Self::Unsupported(name) => name,
        }
    }

    #[must_use]
    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::SyntheticSoftwareV1)
    }
}

impl Serialize for HardwareRailgunAccountCustodyBackend {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HardwareRailgunAccountCustodyBackend {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let backend = String::deserialize(deserializer)?;
        match backend.as_str() {
            Self::SYNTHETIC_SOFTWARE_V1 => Ok(Self::SyntheticSoftwareV1),
            Self::NATIVE_RAILGUN_V1 => Ok(Self::NativeRailgunV1),
            unknown if unknown.trim().is_empty() => Err(de::Error::custom(
                "hardware custody backend must not be empty",
            )),
            unknown => Ok(Self::Unsupported(unknown.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareRailgunAccountMetadata {
    pub profile_id: String,
    pub account_index: u32,
    pub label: String,
    pub descriptor: HardwareDerivationDescriptor,
    pub account_identity: HardwareRailgunAccountIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_address: Option<String>,
    pub custody_backend: HardwareRailgunAccountCustodyBackend,
}

impl HardwareRailgunAccountMetadata {
    #[must_use]
    pub fn synthetic_software_v1(
        profile_id: impl Into<String>,
        account_index: u32,
        label: impl Into<String>,
        descriptor: HardwareDerivationDescriptor,
        account_identity: HardwareRailgunAccountIdentity,
    ) -> Self {
        Self {
            profile_id: profile_id.into(),
            account_index,
            label: label.into(),
            descriptor,
            account_identity,
            receive_address: None,
            custody_backend: HardwareRailgunAccountCustodyBackend::SyntheticSoftwareV1,
        }
    }

    #[must_use]
    pub fn with_receive_address(mut self, receive_address: impl Into<String>) -> Self {
        self.receive_address = Some(receive_address.into());
        self
    }
}

#[must_use]
pub fn hardware_profile_id_for_binding(
    device_kind: HardwareDeviceKind,
    profile_fingerprint: &str,
) -> String {
    let mut hasher = super::Sha256::new();
    hasher.update(b"railgun:hardware-profile:v1");
    hasher.update([0]);
    hasher.update(device_kind.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(profile_fingerprint.as_bytes());
    alloy::hex::encode(hasher.finalize())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct HardwareWalletAccountIndexReservation {
    pub(super) profile: HardwareWalletProfile,
    pub(super) account_index: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PublicAccountSource {
    Derived,
    HardwareDerived,
    Imported,
    ExecutorDerived(ExecutorPublicAccountSource),
}

/// A reference to an account retained by the scoped private wallet. Its chain
/// selects the derivation path even when another network is selected in the UI.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutorPublicAccountSource {
    pub(crate) chain_id: u64,
    pub(crate) index: u32,
    pub(crate) operation: super::ExecutorOperationId,
}

impl ExecutorPublicAccountSource {
    #[must_use]
    pub const fn chain_id(self) -> u64 {
        self.chain_id
    }

    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn operation(self) -> super::ExecutorOperationId {
        self.operation
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PublicAccountScope {
    PrivateWallet { wallet_uuid: String },
    Global,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PublicAccountStatus {
    Active,
    #[serde(alias = "Hidden")]
    Inactive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicAccountMetadata {
    pub public_account_uuid: String,
    pub address: Address,
    pub label: Option<String>,
    pub source: PublicAccountSource,
    pub scope: PublicAccountScope,
    pub derivation_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_descriptor: Option<HardwarePublicAccountDescriptor>,
    pub status: PublicAccountStatus,
    pub display_order: u32,
}

impl PublicAccountMetadata {
    #[must_use]
    pub const fn is_available_on_chain(&self, chain_id: u64) -> bool {
        match self.source {
            PublicAccountSource::ExecutorDerived(source) => source.chain_id == chain_id,
            _ => true,
        }
    }

    #[must_use]
    pub fn is_active_for_wallet(&self, wallet_uuid: &str) -> bool {
        self.status == PublicAccountStatus::Active && self.is_scoped_to_wallet(wallet_uuid)
    }

    #[must_use]
    pub fn is_scoped_to_wallet(&self, wallet_uuid: &str) -> bool {
        match &self.scope {
            PublicAccountScope::PrivateWallet {
                wallet_uuid: scoped,
            } => scoped == wallet_uuid,
            PublicAccountScope::Global => true,
        }
    }

    #[must_use]
    pub const fn is_global(&self) -> bool {
        matches!(self.scope, PublicAccountScope::Global)
    }
}

struct RedactedSecret;

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize)]
#[zeroize(drop)]
pub struct WalletConnectRelayIdentity {
    pub signing_key: [u8; KEY_LEN],
    pub client_id: String,
}

impl fmt::Debug for WalletConnectRelayIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalletConnectRelayIdentity")
            .field("signing_key", &RedactedSecret)
            .field("client_id", &self.client_id)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletConnectPeerMetadata {
    pub name: String,
    pub description: String,
    pub url: String,
    #[serde(default)]
    pub icons: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WalletConnectApprovedNamespace {
    pub chains: Vec<String>,
    pub accounts: Vec<String>,
    pub methods: Vec<String>,
    pub events: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Zeroize)]
#[zeroize(drop)]
pub struct WalletConnectSessionKeys {
    pub sym_key: [u8; KEY_LEN],
    pub responder_private_key: [u8; KEY_LEN],
    pub responder_public_key: [u8; KEY_LEN],
}

impl fmt::Debug for WalletConnectSessionKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalletConnectSessionKeys")
            .field("sym_key", &RedactedSecret)
            .field("responder_private_key", &RedactedSecret)
            .field("responder_public_key", &RedactedSecret)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WalletConnectSessionLifecycleState {
    Active,
    TemporarilyPaused,
    Invalid,
    Disconnected,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalletConnectSessionRecord {
    pub session_uuid: String,
    pub pairing_topic: String,
    pub session_topic: String,
    pub relay_protocol: String,
    pub relay_client_id: String,
    pub peer_metadata: WalletConnectPeerMetadata,
    pub approved_namespaces: BTreeMap<String, WalletConnectApprovedNamespace>,
    pub selected_public_account_uuid: String,
    pub selected_public_account_scope: PublicAccountScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_private_wallet_uuid: Option<String>,
    pub keys: WalletConnectSessionKeys,
    pub expiry_timestamp: u64,
    pub lifecycle_state: WalletConnectSessionLifecycleState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletConnectSessionAccountResolution {
    Usable(PublicAccountMetadata),
    TemporarilyPausedWrongPrivateWallet { owning_wallet_uuid: String },
    InvalidPublicAccount,
}

#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct PublicAccountSecret {
    pub private_key: [u8; KEY_LEN],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrivateAddressBookEntry {
    pub entry_uuid: String,
    pub label: String,
    pub address: String,
    pub display_order: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicAddressBookEntry {
    pub entry_uuid: String,
    pub label: String,
    pub address: Address,
    pub display_order: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BroadcasterPreferenceEntry {
    pub address: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BroadcasterPreferences {
    pub favorites: Vec<BroadcasterPreferenceEntry>,
    pub banned: Vec<BroadcasterPreferenceEntry>,
}

#[derive(Deserialize)]
pub(super) struct WalletMetadataWire {
    pub(super) wallet_uuid: String,
    pub(super) label: String,
    pub(super) derivation_index: u32,
    #[serde(default)]
    pub(super) source: Option<WalletSource>,
    #[serde(default)]
    pub(super) status: Option<WalletStatus>,
    #[serde(default)]
    pub(super) display_order: Option<u32>,
    #[serde(default)]
    pub(super) hardware_descriptor: Option<HardwareDerivationDescriptor>,
    #[serde(default)]
    pub(super) hardware_account: Option<HardwareRailgunAccountMetadata>,
    #[serde(default)]
    pub(super) pending_create_new_chain_ids: BTreeSet<u64>,
    #[serde(default)]
    pub(super) software_context: Option<WalletSoftwareContext>,
}

pub(super) struct DecodedWalletMetadata {
    pub(super) metadata: WalletMetadataBundle,
    pub(super) missing_lifecycle_fields: bool,
    pub(super) missing_display_order: bool,
    pub(super) missing_software_context: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct WalletChainMetadataBundle {
    pub wallet_chain_uuid: String,
    pub wallet_uuid: String,
    pub chain_type: u8,
    pub chain_id: u64,
    pub contract: String,
    pub start_block: u64,
    pub last_scanned_block: u64,
    pub last_scanned_block_hash: Option<[u8; KEY_LEN]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poi_read_source: Option<String>,
}
