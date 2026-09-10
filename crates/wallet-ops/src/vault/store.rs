use super::{
    Arc, BROADCASTER_BANNED_PREFIX, BROADCASTER_FAVORITE_PREFIX, BTreeSet,
    BroadcasterAddressIdentity, BroadcasterPreferenceEntry, BroadcasterPreferences,
    ConfirmedHardwarePublicAccount, CreateSoftwareContextResult, CreatedVault, DbConfig, DbStore,
    DesktopVaultStore, DesktopViewSession, EncryptedRecord, GeneratedSeedMaterial,
    HARDWARE_PROFILE_PREFIX, HARDWARE_WALLET_ACCOUNT_INDEX_PREFIX, HardwareDerivationDescriptor,
    HardwareDeviceKind, HardwareProfileBinding, HardwareProfileBindingKind,
    HardwareProfileMetadata, HardwareProfileSession, HardwareRailgunAccountIdentity,
    HardwareRailgunAccountMetadata, HardwareViewAccessKey, HardwareWalletAccountIndexReservation,
    HardwareWalletProfile, HardwareWalletSyncIntent, KEY_LEN, KdfParams, LoadedWalletMetadata,
    MAX_HARDWARE_RECOVERY_RANGE_COUNT, PRIVATE_ADDRESS_BOOK_PREFIX, PUBLIC_ACCOUNT_METADATA_PREFIX,
    PUBLIC_ADDRESS_BOOK_PREFIX, PathBuf, PrivateAddressBookEntry, ProtectedSoftwareSeedSession,
    PublicAccountMetadata, PublicAccountScope, PublicAccountSecret, PublicAccountSource,
    PublicAccountStatus, PublicAddressBookEntry, RecordKind, Serialize, SoftwareContextChainInput,
    SoftwareContextSyncIntent, SoftwareRailgunSpendSigner, SoftwareSeedSessionBinding, SpendGrant,
    SpendUnlock, StoredHardwareWalletRecord, StoredWalletContextRecord, StoredWalletRecord, U256,
    VAULT_METADATA_KEY, VaultError, VaultMetadata, VaultRecordEntries, VaultSessionId, ViewUnlock,
    ViewingKeyData, WALLET_CHAIN_INDEX_COMPLETE_VERSION, WALLET_CHAIN_INDEX_PREFIX,
    WALLET_CHAIN_METADATA_PREFIX, WALLET_VIEW_PREFIX, WALLETCONNECT_RELAY_IDENTITY_PREFIX,
    WALLETCONNECT_SESSION_PREFIX, WalletCacheKey, WalletChainMetadataBundle,
    WalletConnectRelayIdentity, WalletConnectSessionAccountResolution,
    WalletConnectSessionLifecycleState, WalletConnectSessionRecord, WalletKeys, WalletMeta,
    WalletMetadataBundle, WalletPrivateNamespaceId, WalletSoftwareContext,
    WalletSoftwareContextKind, WalletSource, WalletSpendBundle, WalletSpendSource, WalletStatus,
    WalletViewBundle, Zeroizing, assign_missing_display_orders, bip39_entropy_from_mnemonic,
    bip39_mnemonic_from_entropy, bip39_seed_from_mnemonic_zeroizing,
    broadcaster_banned_record_entry, broadcaster_banned_record_key,
    broadcaster_favorite_record_entry, broadcaster_favorite_record_key,
    broadcaster_preference_entry_identity, create_spend_grant, create_with_params,
    current_vault_version, default_wallet_label_for_metadata,
    derive_public_evm_address_from_entropy, derive_public_evm_address_from_seed,
    derive_public_evm_private_key_from_entropy, derive_public_evm_private_key_from_seed,
    deserialize_wallet_utxo, ensure_private_address_book_address_available,
    ensure_private_address_book_address_available_for_update,
    ensure_public_account_address_available, ensure_public_address_book_address_available,
    ensure_public_address_book_address_available_for_update, fill, generate_opaque_id,
    hardware_profile_record_entry, hardware_wallet_account_index_record_entry,
    initial_derived_public_account, initial_derived_public_account_from_seed,
    next_derived_public_account_index, next_private_address_book_display_order,
    next_public_account_display_order, next_public_address_book_display_order,
    next_wallet_display_order, normalize_public_account_label, parse_public_evm_private_key,
    private_address_book_record_entry, private_address_book_record_key,
    public_account_metadata_record_entry, public_account_metadata_record_key,
    public_account_secret_record_entry, public_account_secret_record_key,
    public_address_book_record_entry, public_address_book_record_key,
    public_evm_address_from_private_key, reencrypt_metadata, serialize_wallet_utxo,
    sort_broadcaster_preference_entries, sort_hardware_profile_metadata,
    sort_private_address_book_entries, sort_public_account_metadata,
    sort_public_address_book_entries, sort_wallet_metadata, sort_walletconnect_sessions,
    unlock_spend, unlock_view, validate_address_book_label,
    validate_broadcaster_preference_address, validate_private_address_book_address,
    validate_public_address_book_address, validate_wallet_label, vault_error_from_wallet_cache,
    wallet_chain_index_complete_record_entry, wallet_chain_index_complete_record_key,
    wallet_chain_index_prefix, wallet_chain_index_record_key, wallet_chain_metadata_record_key,
    wallet_keys_from_mnemonic, wallet_keys_from_seed, wallet_metadata_record_entry,
    wallet_metadata_record_key, wallet_spend_record_key, wallet_utxo_stable_identity,
    wallet_view_record_key, walletconnect_relay_identity_record_entry,
    walletconnect_relay_identity_record_key, walletconnect_session_record_entry,
    walletconnect_session_record_key, zeroize_wallet_keys,
};

mod address_book;
mod base;
mod broadcaster_preferences;
mod chain_cache;
mod gateway;
mod hardware;
mod key_export;
mod public_accounts;
mod record_io;
mod software_context;
mod wallet_metadata;
mod walletconnect;
mod wallets;

pub use gateway::GatewayPermission;
pub use software_context::SoftwareContextMatch;
