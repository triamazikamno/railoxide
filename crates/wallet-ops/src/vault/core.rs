use super::{
    Arc, CacheKeys, DbStore, Deserialize, Error, HardwareProfileSession, Hmac, KeyError, Mutex,
    RailgunSpendSigner, Serialize, Sha256, SpendUnlock, U256, ViewUnlock, ViewingKeyData,
    WalletChainMetadataBundle, WalletKeys, Zeroize, Zeroizing, fill,
};

pub(super) const VAULT_VERSION: u32 = 2;
pub(super) const KEY_LEN: usize = 32;
pub(super) const SALT_LEN: usize = 16;
pub(super) const NONCE_LEN: usize = 24;
pub(super) const DEFAULT_MEMORY_COST_KIB: u32 = 64 * 1024;
pub(super) const DEFAULT_TIME_COST: u32 = 3;
pub(super) const DEFAULT_PARALLELISM: u32 = 1;
pub(super) const VAULT_AAD_PREFIX: &[u8] = b"railgun-desktop-wallet-vault";
pub(super) const VAULT_METADATA_KEY: &str = "vault|meta";
pub(super) const WALLET_METADATA_PREFIX: &str = "wallet-meta|";
pub(super) const WALLET_VIEW_PREFIX: &str = "wallet-view|";
pub(super) const WALLET_SPEND_PREFIX: &str = "wallet-spend|";
pub(super) const WALLET_CHAIN_METADATA_PREFIX: &str = "wallet-chain-meta|";
pub(super) const WALLET_CHAIN_INDEX_PREFIX: &str = "wallet-chain-index|";
pub(super) const WALLET_CHAIN_INDEX_COMPLETE_PREFIX: &str = "wallet-chain-index-complete|";
pub(super) const WALLET_CHAIN_INDEX_COMPLETE_VERSION: u8 = 1;
pub(super) const HARDWARE_PROFILE_PREFIX: &str = "hardware-profile|";
pub(super) const HARDWARE_WALLET_ACCOUNT_INDEX_PREFIX: &str = "hardware-wallet-account-index|";
pub const MAX_HARDWARE_RECOVERY_RANGE_COUNT: u32 = 255;
pub(super) const PUBLIC_ACCOUNT_METADATA_PREFIX: &str = "public-account-meta|";
pub(super) const PUBLIC_ACCOUNT_SECRET_PREFIX: &str = "public-account-secret|";
pub(super) const PRIVATE_ADDRESS_BOOK_PREFIX: &str = "private-address-book|";
pub(super) const PUBLIC_ADDRESS_BOOK_PREFIX: &str = "public-address-book|";
pub(super) const BROADCASTER_FAVORITE_PREFIX: &str = "broadcaster-favorite|";
pub(super) const BROADCASTER_BANNED_PREFIX: &str = "broadcaster-banned|";
pub(super) const WALLETCONNECT_RELAY_IDENTITY_PREFIX: &str = "walletconnect-relay-identity|";
pub(super) const WALLETCONNECT_SESSION_PREFIX: &str = "walletconnect-session|";
pub const PRIMARY_WALLET_LABEL: &str = "Primary wallet";
pub(super) const ADDITIONAL_WALLET_LABEL_PREFIX: &str = "Wallet ";
pub(super) type HmacSha256 = Hmac<Sha256>;
pub(super) type VaultRecordEntries = Vec<(String, Vec<u8>)>;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("random generation failed")]
    Random,
    #[error("invalid key derivation parameters")]
    InvalidKdfParams,
    #[error("key derivation failed")]
    Kdf,
    #[error("key separation failed")]
    KeySeparation,
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed")]
    Decrypt,
    #[error("wallet-private migration conflict for {kind}: {reason}")]
    WalletPrivateMigrationConflict {
        kind: &'static str,
        reason: &'static str,
    },
    #[error("encode failed: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode failed: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("db failed: {0}")]
    Db(#[from] local_db::DbError),
    #[error("invalid wallet cache key: {0}")]
    InvalidWalletCacheKey(#[from] local_db::WalletCacheKeyError),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("wallet key failed: {0}")]
    Key(#[from] KeyError),
    #[error("unsupported vault version {0}")]
    UnsupportedVersion(u32),
    #[error("vault already exists")]
    VaultAlreadyExists,
    #[error("vault not found")]
    VaultNotFound,
    #[error("unlock failed")]
    UnlockFailed,
    #[error("spend grant is invalid")]
    InvalidSpendGrant,
    #[error("invalid gateway permission")]
    InvalidGatewayPermission,
    #[error("software seed session binding mismatch")]
    SoftwareSeedSessionBindingMismatch,
    #[error("protected software seed session is required")]
    SoftwareSeedSessionRequired,
    #[error("invalid software seed length")]
    InvalidSoftwareSeedLength,
    #[error("duplicate software context identity")]
    DuplicateSoftwareContextIdentity,
    #[error("invalid software context identity")]
    InvalidSoftwareContextIdentity,
    #[error("passphrase confirmation does not match")]
    PassphraseConfirmationMismatch,
    #[error("software context passphrase cannot be empty")]
    EmptySoftwareContextPassphrase,
    #[error("software context safe head is unavailable")]
    SoftwareContextSafeHeadUnavailable,
    #[error("software context safe head overflow")]
    SoftwareContextSafeHeadOverflow,
    #[error("wallet UUID already exists")]
    DuplicateWalletUuid,
    #[error("cannot delete a standard wallet while passphrase children remain")]
    StandardWalletHasPassphraseChildren,
    #[error("duplicate software context chain input")]
    DuplicateSoftwareContextChainInput,
    #[error("wallet not found")]
    WalletNotFound,
    #[error("invalid wallet metadata")]
    InvalidWalletMetadata,
    #[error("wallet label cannot be empty")]
    InvalidWalletLabel,
    #[error("wallet label already exists")]
    DuplicateWalletLabel,
    #[error("wallet order does not match active wallets")]
    InvalidWalletOrder,
    #[error("cannot deactivate the only active wallet")]
    LastActiveWallet,
    #[error("wallet display order overflow")]
    WalletDisplayOrderOverflow,
    #[error("public account not found")]
    PublicAccountNotFound,
    #[error("public account address already exists")]
    DuplicatePublicAccountAddress,
    #[error("invalid public account operation")]
    InvalidPublicAccountOperation,
    #[error("public account display order overflow")]
    PublicAccountDisplayOrderOverflow,
    #[error("invalid public EVM private key")]
    InvalidPublicEvmPrivateKey,
    #[error("public EVM key derivation failed")]
    PublicEvmKeyDerivation,
    #[error("address book label cannot be empty")]
    InvalidAddressBookLabel,
    #[error("invalid private address book recipient")]
    InvalidPrivateAddressBookAddress,
    #[error("private address book recipient already exists")]
    DuplicatePrivateAddressBookAddress,
    #[error("private address book entry not found")]
    PrivateAddressBookEntryNotFound,
    #[error("private address book display order overflow")]
    PrivateAddressBookDisplayOrderOverflow,
    #[error("invalid public address book recipient")]
    InvalidPublicAddressBookAddress,
    #[error("public address book recipient already exists")]
    DuplicatePublicAddressBookAddress,
    #[error("public address book entry not found")]
    PublicAddressBookEntryNotFound,
    #[error("public address book display order overflow")]
    PublicAddressBookDisplayOrderOverflow,
    #[error("invalid broadcaster preference address")]
    InvalidBroadcasterPreferenceAddress,
    #[error("invalid hardware wallet descriptor")]
    InvalidHardwareWalletDescriptor,
    #[error("hardware wallet account index overflow")]
    HardwareWalletAccountIndexOverflow,
    #[error("hardware wallet account index already exists")]
    DuplicateHardwareWalletAccountIndex,
    #[error("derived hardware wallet key does not match the stored wallet")]
    HardwareWalletIdentityMismatch,
    #[error("hardware wallet view requires device unlock")]
    HardwareWalletViewRequiresDevice,
    #[error("wallet-chain metadata ownership is unavailable")]
    WalletChainMetadataUnavailable,
    #[error("wallet mnemonic is unavailable")]
    WalletMnemonicUnavailable,
    #[error("unsupported hardware custody backend {0}")]
    UnsupportedHardwareCustodyBackend(String),
    #[error("invalid hardware account recovery range")]
    InvalidHardwareAccountRecoveryRange,
    #[error("derive hardware wallet receive address failed")]
    HardwareWalletReceiveAddress,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct KdfParams {
    pub memory_cost_kib: u32,
    pub time_cost: u32,
    pub parallelism: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            memory_cost_kib: DEFAULT_MEMORY_COST_KIB,
            time_cost: DEFAULT_TIME_COST,
            parallelism: DEFAULT_PARALLELISM,
        }
    }
}

impl KdfParams {
    #[must_use]
    pub const fn new(memory_cost_kib: u32, time_cost: u32, parallelism: u32) -> Self {
        Self {
            memory_cost_kib,
            time_cost,
            parallelism,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptedRecord {
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

impl EncryptedRecord {
    pub fn to_record_entry(&self, key: String) -> Result<(String, Vec<u8>), VaultError> {
        Ok((key, rmp_serde::to_vec_named(self)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultMetadata {
    pub version: u32,
    pub kdf: KdfParams,
    pub salt: [u8; SALT_LEN],
    pub wrapped_view_dek: EncryptedRecord,
    pub wrapped_spend_dek: EncryptedRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    ViewDek,
    SpendDek,
    WalletViewBundle,
    WalletSpendBundle,
    WalletMetadata,
    WalletChainMetadata,
    WalletCacheRow,
    PendingOutputPoiContext,
    OutputPoiRecovery,
    SenderTransactionCandidate,
    HardwareProfileMetadata,
    HardwareWalletAccountIndexReservation,
    PublicAccountMetadata,
    PublicAccountSecret,
    PrivateAddressBookEntry,
    PublicAddressBookEntry,
    BroadcasterFavoriteEntry,
    BroadcasterBannedEntry,
    WalletConnectRelayIdentity,
    WalletConnectSession,
    SoftwareContextSeed,
    GatewayPermission,
    ExecutorAllocation,
    ExecutorOperation,
}

impl RecordKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ViewDek => "view-dek",
            Self::SpendDek => "spend-dek",
            Self::WalletViewBundle => "wallet-view-bundle",
            Self::WalletSpendBundle => "wallet-spend-bundle",
            Self::WalletMetadata => "wallet-metadata",
            Self::WalletChainMetadata => "wallet-chain-metadata",
            Self::WalletCacheRow => "wallet-cache-row",
            Self::PendingOutputPoiContext => "pending-output-poi-context",
            Self::OutputPoiRecovery => "output-poi-recovery",
            Self::SenderTransactionCandidate => "sender-transaction-candidate",
            Self::HardwareProfileMetadata => "hardware-profile-metadata",
            Self::HardwareWalletAccountIndexReservation => {
                "hardware-wallet-account-index-reservation"
            }
            Self::PublicAccountMetadata => "public-account-metadata",
            Self::PublicAccountSecret => "public-account-secret",
            Self::PrivateAddressBookEntry => "private-address-book-entry",
            Self::PublicAddressBookEntry => "public-address-book-entry",
            Self::BroadcasterFavoriteEntry => "broadcaster-favorite-entry",
            Self::BroadcasterBannedEntry => "broadcaster-banned-entry",
            Self::WalletConnectRelayIdentity => "walletconnect-relay-identity",
            Self::WalletConnectSession => "walletconnect-session",
            Self::SoftwareContextSeed => "software-context-seed",
            Self::GatewayPermission => "gateway-permission",
            Self::ExecutorAllocation => "executor-allocation",
            Self::ExecutorOperation => "executor-operation",
        }
    }

    #[must_use]
    pub fn aad(self, record_id: &str) -> Vec<u8> {
        let mut aad =
            Vec::with_capacity(VAULT_AAD_PREFIX.len() + self.as_str().len() + record_id.len() + 16);
        aad.extend_from_slice(VAULT_AAD_PREFIX);
        aad.extend_from_slice(b":v1:");
        aad.extend_from_slice(self.as_str().as_bytes());
        aad.extend_from_slice(b":");
        aad.extend_from_slice(record_id.as_bytes());
        aad
    }
}

pub struct SecretKey(pub(super) Zeroizing<[u8; KEY_LEN]>);

impl SecretKey {
    pub(super) fn random() -> Result<Self, VaultError> {
        let mut key = [0u8; KEY_LEN];
        fill(&mut key).map_err(|_| VaultError::Random)?;
        Ok(Self(Zeroizing::new(key)))
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(super) fn from_zeroizing_vec(bytes: Zeroizing<Vec<u8>>) -> Result<Self, VaultError> {
        if bytes.len() != KEY_LEN {
            return Err(VaultError::UnlockFailed);
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        Ok(Self(Zeroizing::new(key)))
    }

    #[must_use]
    pub fn expose_secret(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    pub(super) fn clone_secret(&self) -> Self {
        Self(Zeroizing::new(*self.expose_secret()))
    }
}

pub struct CreatedVault {
    pub metadata: VaultMetadata,
    pub view: ViewUnlock,
    pub spend: SpendUnlock,
}

pub struct DesktopVaultStore {
    pub(super) db: Arc<DbStore>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWalletRecord {
    pub wallet_id: String,
    pub derivation_index: u32,
    pub view_record_key: String,
    pub spend_record_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWalletContextRecord {
    pub wallet_id: String,
    pub derivation_index: u32,
    pub view_record_key: String,
    pub metadata_record_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHardwareWalletRecord {
    pub wallet_id: String,
    pub derivation_index: u32,
    pub view_record_key: String,
    pub metadata_record_key: String,
}

pub struct GeneratedSeedMaterial {
    pub mnemonic: Zeroizing<String>,
    pub entropy: Zeroizing<Vec<u8>>,
}

pub struct DesktopViewSession {
    pub(super) session_identity: Arc<()>,
    pub(super) wallet_id: String,
    pub(super) derivation_index: u32,
    pub(super) spending_public_key: [U256; 2],
    pub(super) scan_keys: ViewingKeyData,
    pub(super) view: ViewUnlock,
    pub(super) private_view: ViewUnlock,
    pub(super) hardware_profile_session: Option<HardwareProfileSession>,
}

pub struct SoftwareRailgunSpendSigner {
    pub(super) wallet: WalletKeys,
}

pub struct DesktopEncryptedWalletCacheStore {
    pub(super) db: Arc<DbStore>,
    pub(super) metadata: Mutex<WalletChainMetadataBundle>,
    pub(super) cache_keys: CacheKeys,
}

impl RailgunSpendSigner for SoftwareRailgunSpendSigner {
    fn spending_public_key(&self) -> [U256; 2] {
        self.wallet.spending_public_key()
    }

    fn sign_spend_message(&self, msg: U256) -> [U256; 3] {
        self.wallet.sign_spend_message(msg)
    }
}

impl Drop for SoftwareRailgunSpendSigner {
    fn drop(&mut self) {
        self.wallet.spending_private_key.zeroize();
        self.wallet.viewing.viewing_private_key.zeroize();
    }
}
