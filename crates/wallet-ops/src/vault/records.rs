use super::{
    ADDITIONAL_WALLET_LABEL_PREFIX, Address, BROADCASTER_BANNED_PREFIX,
    BROADCASTER_FAVORITE_PREFIX, BTreeSet, BroadcasterPreferenceEntry, HARDWARE_PROFILE_PREFIX,
    HARDWARE_WALLET_ACCOUNT_INDEX_PREFIX, HardwareProfileMetadata,
    HardwareWalletAccountIndexReservation, KEY_LEN, KeyError, PRIMARY_WALLET_LABEL,
    PRIVATE_ADDRESS_BOOK_PREFIX, PUBLIC_ACCOUNT_METADATA_PREFIX, PUBLIC_ACCOUNT_SECRET_PREFIX,
    PUBLIC_ADDRESS_BOOK_PREFIX, PrivateAddressBookEntry, PrivateKeySigner, PublicAccountMetadata,
    PublicAccountScope, PublicAccountSecret, PublicAccountSource, PublicAccountStatus,
    PublicAddressBookEntry, RecordKind, SigningKey, SpendUnlock, U256, VaultError, ViewUnlock,
    WALLET_CHAIN_INDEX_COMPLETE_PREFIX, WALLET_CHAIN_INDEX_COMPLETE_VERSION,
    WALLET_CHAIN_INDEX_PREFIX, WALLET_CHAIN_METADATA_PREFIX, WALLET_METADATA_PREFIX,
    WALLET_SPEND_PREFIX, WALLET_VIEW_PREFIX, WALLETCONNECT_RELAY_IDENTITY_PREFIX,
    WALLETCONNECT_SESSION_PREFIX, WalletCacheError, WalletConnectRelayIdentity,
    WalletConnectSessionRecord, WalletKeys, WalletMetadataBundle, WalletUtxo, Zeroize, Zeroizing,
    bip39_mnemonic_from_entropy, generate_opaque_id,
};

use coins_bip32::prelude::{Parent, XPriv, XPub};

use crate::parse_railgun_recipient;

pub fn bip39_seed_from_mnemonic(
    mnemonic: &str,
    passphrase: &str,
) -> Result<Zeroizing<[u8; 64]>, VaultError> {
    let passphrase = Zeroizing::new(passphrase.to_owned());
    bip39_seed_from_mnemonic_zeroizing(mnemonic, &passphrase)
}

pub(super) fn bip39_seed_from_mnemonic_zeroizing(
    mnemonic: &str,
    passphrase: &Zeroizing<String>,
) -> Result<Zeroizing<[u8; 64]>, VaultError> {
    let mnemonic =
        bip39::Mnemonic::parse(mnemonic).map_err(|_| VaultError::Key(KeyError::InvalidMnemonic))?;
    Ok(Zeroizing::new(mnemonic.to_seed(passphrase.as_str())))
}

pub(super) fn wallet_keys_from_seed(
    seed: &[u8; 64],
    derivation_index: u32,
) -> Result<WalletKeys, VaultError> {
    Ok(WalletKeys::from_seed(seed, derivation_index)?)
}

pub fn wallet_keys_from_mnemonic(
    mnemonic: &str,
    passphrase: &str,
    derivation_index: u32,
) -> Result<WalletKeys, VaultError> {
    let seed = bip39_seed_from_mnemonic(mnemonic, passphrase)?;
    wallet_keys_from_seed(&seed, derivation_index)
}

pub(super) fn zeroize_wallet_keys(wallet: &mut WalletKeys) {
    wallet.spending_private_key.zeroize();
    wallet.viewing.viewing_private_key.zeroize();
}

pub(super) fn wallet_view_record_key(wallet_id: &str) -> String {
    format!("{WALLET_VIEW_PREFIX}{wallet_id}")
}

pub(super) fn wallet_spend_record_key(wallet_id: &str) -> String {
    format!("{WALLET_SPEND_PREFIX}{wallet_id}")
}

pub(super) fn wallet_metadata_record_key(wallet_uuid: &str) -> String {
    format!("{WALLET_METADATA_PREFIX}{wallet_uuid}")
}

pub(super) fn wallet_chain_metadata_record_key(wallet_chain_uuid: &str) -> String {
    format!("{WALLET_CHAIN_METADATA_PREFIX}{wallet_chain_uuid}")
}

pub(super) fn wallet_chain_index_prefix(wallet_uuid: &str) -> String {
    format!(
        "{WALLET_CHAIN_INDEX_PREFIX}{}|",
        alloy::hex::encode(wallet_uuid.as_bytes())
    )
}

pub(super) fn wallet_chain_index_record_key(wallet_uuid: &str, wallet_chain_uuid: &str) -> String {
    format!(
        "{}{wallet_chain_uuid}",
        wallet_chain_index_prefix(wallet_uuid)
    )
}

pub(super) fn wallet_chain_index_complete_record_key(wallet_uuid: &str) -> String {
    format!(
        "{WALLET_CHAIN_INDEX_COMPLETE_PREFIX}{}",
        alloy::hex::encode(wallet_uuid.as_bytes())
    )
}

pub(super) fn wallet_chain_index_complete_record_entry(
    wallet_uuid: &str,
) -> Result<(String, Vec<u8>), VaultError> {
    Ok((
        wallet_chain_index_complete_record_key(wallet_uuid),
        rmp_serde::to_vec_named(&WALLET_CHAIN_INDEX_COMPLETE_VERSION)?,
    ))
}

pub(super) fn hardware_wallet_account_index_record_key(reservation_uuid: &str) -> String {
    format!("{HARDWARE_WALLET_ACCOUNT_INDEX_PREFIX}{reservation_uuid}")
}

pub(super) fn hardware_profile_record_key(profile_id: &str) -> String {
    format!("{HARDWARE_PROFILE_PREFIX}{profile_id}")
}

pub(super) fn public_account_metadata_record_key(public_account_uuid: &str) -> String {
    format!("{PUBLIC_ACCOUNT_METADATA_PREFIX}{public_account_uuid}")
}

pub(super) fn public_account_secret_record_key(public_account_uuid: &str) -> String {
    format!("{PUBLIC_ACCOUNT_SECRET_PREFIX}{public_account_uuid}")
}

pub(super) fn private_address_book_record_key(entry_uuid: &str) -> String {
    format!("{PRIVATE_ADDRESS_BOOK_PREFIX}{entry_uuid}")
}

pub(super) fn public_address_book_record_key(entry_uuid: &str) -> String {
    format!("{PUBLIC_ADDRESS_BOOK_PREFIX}{entry_uuid}")
}

pub(super) fn walletconnect_relay_identity_record_key(wallet_uuid: &str) -> String {
    format!("{WALLETCONNECT_RELAY_IDENTITY_PREFIX}{wallet_uuid}")
}

pub(super) fn walletconnect_session_record_key(session_uuid: &str) -> String {
    format!("{WALLETCONNECT_SESSION_PREFIX}{session_uuid}")
}

pub(super) fn broadcaster_favorite_record_key(entry_uuid: &str) -> String {
    format!("{BROADCASTER_FAVORITE_PREFIX}{entry_uuid}")
}

pub(super) fn broadcaster_banned_record_key(entry_uuid: &str) -> String {
    format!("{BROADCASTER_BANNED_PREFIX}{entry_uuid}")
}

pub(super) fn wallet_cache_counts(utxos: &[WalletUtxo]) -> (usize, usize) {
    let spent = utxos.iter().filter(|utxo| utxo.is_spent()).count();
    (utxos.len().saturating_sub(spent), spent)
}

pub(super) fn vault_error_from_wallet_cache(error: WalletCacheError) -> VaultError {
    match error {
        WalletCacheError::Encode(error) => VaultError::Encode(error),
        WalletCacheError::Decode(error) => VaultError::Decode(error),
        WalletCacheError::Db(error) => VaultError::Db(error),
        WalletCacheError::Io(error) => VaultError::Io(error),
        WalletCacheError::Crypto => VaultError::Decrypt,
    }
}

pub(super) struct LoadedWalletMetadata {
    pub(super) metadata: WalletMetadataBundle,
    pub(super) needs_persist: bool,
    pub(super) missing_display_order: bool,
}

pub(super) fn wallet_metadata_record_entry(
    view: &ViewUnlock,
    metadata: &WalletMetadataBundle,
) -> Result<(String, Vec<u8>), VaultError> {
    metadata.validate()?;
    let key = wallet_metadata_record_key(&metadata.wallet_uuid);
    let record = view.encrypt_wallet_metadata(&metadata.wallet_uuid, metadata)?;
    record.to_record_entry(key)
}

pub(super) fn hardware_wallet_account_index_record_entry(
    view: &ViewUnlock,
    reservation_uuid: &str,
    reservation: &HardwareWalletAccountIndexReservation,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = hardware_wallet_account_index_record_key(reservation_uuid);
    let record =
        view.encrypt_hardware_wallet_account_index_reservation(reservation_uuid, reservation)?;
    record.to_record_entry(key)
}

pub(super) fn hardware_profile_record_entry(
    view: &ViewUnlock,
    profile: &HardwareProfileMetadata,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = hardware_profile_record_key(&profile.profile_id);
    let record = view.encrypt_hardware_profile_metadata(&profile.profile_id, profile)?;
    record.to_record_entry(key)
}

pub(super) fn public_account_metadata_record_entry(
    view: &ViewUnlock,
    metadata: &PublicAccountMetadata,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = public_account_metadata_record_key(&metadata.public_account_uuid);
    let record = view.encrypt_public_account_metadata(&metadata.public_account_uuid, metadata)?;
    record.to_record_entry(key)
}

pub(super) fn public_account_secret_record_entry(
    spend: &SpendUnlock,
    metadata: &PublicAccountMetadata,
    secret: &PublicAccountSecret,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = public_account_secret_record_key(&metadata.public_account_uuid);
    let record = spend.encrypt_public_account_secret(&metadata.public_account_uuid, secret)?;
    record.to_record_entry(key)
}

pub(super) fn private_address_book_record_entry(
    view: &ViewUnlock,
    entry: &PrivateAddressBookEntry,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = private_address_book_record_key(&entry.entry_uuid);
    let record = view.encrypt_private_address_book_entry(&entry.entry_uuid, entry)?;
    record.to_record_entry(key)
}

pub(super) fn public_address_book_record_entry(
    view: &ViewUnlock,
    entry: &PublicAddressBookEntry,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = public_address_book_record_key(&entry.entry_uuid);
    let record = view.encrypt_public_address_book_entry(&entry.entry_uuid, entry)?;
    record.to_record_entry(key)
}

pub(super) fn broadcaster_favorite_record_entry(
    view: &ViewUnlock,
    entry_uuid: &str,
    entry: &BroadcasterPreferenceEntry,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = broadcaster_favorite_record_key(entry_uuid);
    let record = view.encrypt_broadcaster_preference_entry(
        RecordKind::BroadcasterFavoriteEntry,
        entry_uuid,
        entry,
    )?;
    record.to_record_entry(key)
}

pub(super) fn broadcaster_banned_record_entry(
    view: &ViewUnlock,
    entry_uuid: &str,
    entry: &BroadcasterPreferenceEntry,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = broadcaster_banned_record_key(entry_uuid);
    let record = view.encrypt_broadcaster_preference_entry(
        RecordKind::BroadcasterBannedEntry,
        entry_uuid,
        entry,
    )?;
    record.to_record_entry(key)
}

pub(super) fn walletconnect_relay_identity_record_entry(
    view: &ViewUnlock,
    wallet_uuid: &str,
    identity: &WalletConnectRelayIdentity,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = walletconnect_relay_identity_record_key(wallet_uuid);
    let record = view.encrypt_walletconnect_relay_identity(wallet_uuid, identity)?;
    record.to_record_entry(key)
}

pub(super) fn walletconnect_session_record_entry(
    view: &ViewUnlock,
    session: &WalletConnectSessionRecord,
) -> Result<(String, Vec<u8>), VaultError> {
    let key = walletconnect_session_record_key(&session.session_uuid);
    let record = view.encrypt_walletconnect_session(&session.session_uuid, session)?;
    record.to_record_entry(key)
}

#[must_use]
pub fn normalize_wallet_label(label: &str) -> String {
    label.trim().to_owned()
}

pub(super) fn wallet_label_duplicate_key(label: &str) -> String {
    normalize_wallet_label(label).to_lowercase()
}

pub fn validate_wallet_label(
    label: &str,
    existing: &[WalletMetadataBundle],
    current_wallet_uuid: Option<&str>,
) -> Result<String, VaultError> {
    let label = normalize_wallet_label(label);
    if label.is_empty() {
        return Err(VaultError::InvalidWalletLabel);
    }
    let label_key = wallet_label_duplicate_key(&label);
    if existing.iter().any(|metadata| {
        current_wallet_uuid != Some(metadata.wallet_uuid.as_str())
            && wallet_label_duplicate_key(&metadata.label) == label_key
    }) {
        return Err(VaultError::DuplicateWalletLabel);
    }
    Ok(label)
}

#[must_use]
pub fn default_wallet_label_for_metadata(metadata: &[WalletMetadataBundle]) -> String {
    if metadata.is_empty() {
        return PRIMARY_WALLET_LABEL.to_owned();
    }

    let used = metadata
        .iter()
        .map(|metadata| wallet_label_duplicate_key(&metadata.label))
        .collect::<BTreeSet<_>>();
    let mut label_index = 2u32;
    loop {
        let label = format!("{ADDITIONAL_WALLET_LABEL_PREFIX}{label_index}");
        if !used.contains(&wallet_label_duplicate_key(&label)) {
            return label;
        }
        label_index = label_index.saturating_add(1);
    }
}

pub(super) fn next_wallet_display_order(
    metadata: &[WalletMetadataBundle],
) -> Result<u32, VaultError> {
    metadata
        .iter()
        .map(|metadata| metadata.display_order)
        .max()
        .map_or(Ok(0), |max_display_order| {
            max_display_order
                .checked_add(1)
                .ok_or(VaultError::WalletDisplayOrderOverflow)
        })
}

pub(super) fn assign_missing_display_orders(
    metadata: &mut [LoadedWalletMetadata],
) -> Result<(), VaultError> {
    let mut next_order = metadata
        .iter()
        .filter(|metadata| !metadata.missing_display_order)
        .map(|metadata| metadata.metadata.display_order)
        .max()
        .map_or(Ok(0), |display_order| {
            display_order
                .checked_add(1)
                .ok_or(VaultError::WalletDisplayOrderOverflow)
        })?;
    let mut missing_indices = metadata
        .iter()
        .enumerate()
        .filter_map(|(index, metadata)| metadata.missing_display_order.then_some(index))
        .collect::<Vec<_>>();
    missing_indices.sort_by(|left, right| {
        metadata[*left]
            .metadata
            .label
            .cmp(&metadata[*right].metadata.label)
            .then_with(|| {
                metadata[*left]
                    .metadata
                    .wallet_uuid
                    .cmp(&metadata[*right].metadata.wallet_uuid)
            })
    });

    for index in missing_indices {
        metadata[index].metadata.display_order = next_order;
        metadata[index].needs_persist = true;
        next_order = next_order
            .checked_add(1)
            .ok_or(VaultError::WalletDisplayOrderOverflow)?;
    }
    Ok(())
}

pub fn sort_wallet_metadata(metadata: &mut [WalletMetadataBundle]) {
    metadata.sort_by(|left, right| {
        left.display_order
            .cmp(&right.display_order)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.wallet_uuid.cmp(&right.wallet_uuid))
    });
}

pub fn sort_hardware_profile_metadata(metadata: &mut [HardwareProfileMetadata]) {
    metadata.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.device_kind.cmp(&right.device_kind))
            .then_with(|| left.profile_id.cmp(&right.profile_id))
    });
}

#[must_use]
pub fn normalize_public_account_label(label: Option<&str>) -> Option<String> {
    label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(ToOwned::to_owned)
}

#[must_use]
pub fn public_account_default_label(label_number: u32) -> String {
    format!("Account #{label_number}")
}

pub fn sort_public_account_metadata(metadata: &mut [PublicAccountMetadata]) {
    metadata.sort_by(|left, right| {
        left.display_order
            .cmp(&right.display_order)
            .then_with(|| left.address.cmp(&right.address))
            .then_with(|| left.public_account_uuid.cmp(&right.public_account_uuid))
    });
}

pub(super) fn next_public_account_display_order(
    metadata: &[PublicAccountMetadata],
) -> Result<u32, VaultError> {
    metadata
        .iter()
        .map(|metadata| metadata.display_order)
        .max()
        .map_or(Ok(0), |max_display_order| {
            max_display_order
                .checked_add(1)
                .ok_or(VaultError::PublicAccountDisplayOrderOverflow)
        })
}

#[must_use]
pub fn normalize_address_book_label(label: &str) -> String {
    label.trim().to_owned()
}

pub fn validate_address_book_label(label: &str) -> Result<String, VaultError> {
    let label = normalize_address_book_label(label);
    if label.is_empty() {
        Err(VaultError::InvalidAddressBookLabel)
    } else {
        Ok(label)
    }
}

pub fn sort_private_address_book_entries(entries: &mut [PrivateAddressBookEntry]) {
    entries.sort_by(|left, right| {
        left.display_order
            .cmp(&right.display_order)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.entry_uuid.cmp(&right.entry_uuid))
    });
}

pub fn sort_public_address_book_entries(entries: &mut [PublicAddressBookEntry]) {
    entries.sort_by(|left, right| {
        left.display_order
            .cmp(&right.display_order)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.entry_uuid.cmp(&right.entry_uuid))
    });
}

pub fn sort_walletconnect_sessions(sessions: &mut [WalletConnectSessionRecord]) {
    sessions.sort_by(|left, right| {
        left.peer_metadata
            .name
            .cmp(&right.peer_metadata.name)
            .then_with(|| left.session_uuid.cmp(&right.session_uuid))
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct BroadcasterAddressIdentity {
    pub(super) master_public_key: U256,
    pub(super) viewing_public_key: [u8; KEY_LEN],
}

pub fn sort_broadcaster_preference_entries(entries: &mut [BroadcasterPreferenceEntry]) {
    entries.sort_by(|left, right| left.address.cmp(&right.address));
}

pub(super) fn next_private_address_book_display_order(
    entries: &[PrivateAddressBookEntry],
) -> Result<u32, VaultError> {
    entries
        .iter()
        .map(|entry| entry.display_order)
        .max()
        .map_or(Ok(0), |max_display_order| {
            max_display_order
                .checked_add(1)
                .ok_or(VaultError::PrivateAddressBookDisplayOrderOverflow)
        })
}

pub(super) fn next_public_address_book_display_order(
    entries: &[PublicAddressBookEntry],
) -> Result<u32, VaultError> {
    entries
        .iter()
        .map(|entry| entry.display_order)
        .max()
        .map_or(Ok(0), |max_display_order| {
            max_display_order
                .checked_add(1)
                .ok_or(VaultError::PublicAddressBookDisplayOrderOverflow)
        })
}

pub(super) fn validate_private_address_book_address(address: &str) -> Result<String, VaultError> {
    let address = address.trim();
    parse_railgun_recipient(address).map_err(|_| VaultError::InvalidPrivateAddressBookAddress)?;
    Ok(address.to_owned())
}

pub(super) fn validate_public_address_book_address(address: &str) -> Result<Address, VaultError> {
    address
        .trim()
        .parse()
        .map_err(|_| VaultError::InvalidPublicAddressBookAddress)
}

pub(super) fn validate_broadcaster_preference_address(
    address: &str,
) -> Result<(String, BroadcasterAddressIdentity), VaultError> {
    let address = address.trim();
    let address_data = parse_railgun_recipient(address)
        .map_err(|_| VaultError::InvalidBroadcasterPreferenceAddress)?;
    Ok((
        address.to_owned(),
        BroadcasterAddressIdentity {
            master_public_key: address_data.master_public_key,
            viewing_public_key: address_data.viewing_public_key,
        },
    ))
}

pub(super) fn broadcaster_preference_entry_identity(
    entry: &BroadcasterPreferenceEntry,
) -> Result<BroadcasterAddressIdentity, VaultError> {
    validate_broadcaster_preference_address(&entry.address).map(|(_, identity)| identity)
}

pub(super) fn ensure_private_address_book_address_available(
    entries: &[PrivateAddressBookEntry],
    existing_recipients: &[String],
    address: &str,
) -> Result<(), VaultError> {
    ensure_private_address_book_address_available_except(
        entries,
        existing_recipients,
        None,
        address,
    )
}

pub(super) fn ensure_private_address_book_address_available_for_update(
    entries: &[PrivateAddressBookEntry],
    existing_recipients: &[String],
    current_entry_uuid: &str,
    address: &str,
) -> Result<(), VaultError> {
    ensure_private_address_book_address_available_except(
        entries,
        existing_recipients,
        Some(current_entry_uuid),
        address,
    )
}

fn ensure_private_address_book_address_available_except(
    entries: &[PrivateAddressBookEntry],
    existing_recipients: &[String],
    current_entry_uuid: Option<&str>,
    address: &str,
) -> Result<(), VaultError> {
    let address_data = parse_railgun_recipient(address)
        .map_err(|_| VaultError::InvalidPrivateAddressBookAddress)?;
    let duplicate_entry = entries.iter().any(|entry| {
        if current_entry_uuid == Some(entry.entry_uuid.as_str()) {
            return false;
        }
        parse_railgun_recipient(&entry.address).is_ok_and(|entry_data| {
            entry_data.master_public_key == address_data.master_public_key
                && entry_data.viewing_public_key == address_data.viewing_public_key
        })
    });
    let duplicate_existing = existing_recipients.iter().any(|recipient| {
        parse_railgun_recipient(recipient).is_ok_and(|recipient_data| {
            recipient_data.master_public_key == address_data.master_public_key
                && recipient_data.viewing_public_key == address_data.viewing_public_key
        })
    });
    if duplicate_entry || duplicate_existing {
        Err(VaultError::DuplicatePrivateAddressBookAddress)
    } else {
        Ok(())
    }
}

pub(super) fn ensure_public_address_book_address_available(
    entries: &[PublicAddressBookEntry],
    existing_accounts: &[PublicAccountMetadata],
    address: Address,
) -> Result<(), VaultError> {
    ensure_public_address_book_address_available_except(entries, existing_accounts, None, address)
}

pub(super) fn ensure_public_address_book_address_available_for_update(
    entries: &[PublicAddressBookEntry],
    existing_accounts: &[PublicAccountMetadata],
    current_entry_uuid: &str,
    address: Address,
) -> Result<(), VaultError> {
    ensure_public_address_book_address_available_except(
        entries,
        existing_accounts,
        Some(current_entry_uuid),
        address,
    )
}

fn ensure_public_address_book_address_available_except(
    entries: &[PublicAddressBookEntry],
    existing_accounts: &[PublicAccountMetadata],
    current_entry_uuid: Option<&str>,
    address: Address,
) -> Result<(), VaultError> {
    if entries.iter().any(|entry| {
        current_entry_uuid != Some(entry.entry_uuid.as_str()) && entry.address == address
    }) || existing_accounts
        .iter()
        .any(|account| account.status == PublicAccountStatus::Active && account.address == address)
    {
        Err(VaultError::DuplicatePublicAddressBookAddress)
    } else {
        Ok(())
    }
}

/// Derivation indexes used by the wallet's derived Public accounts, of any status.
pub(super) fn derived_public_account_indexes<'a>(
    metadata: &'a [PublicAccountMetadata],
    wallet_uuid: &'a str,
) -> impl Iterator<Item = u32> + 'a {
    metadata
        .iter()
        .filter(|account| {
            matches!(
                account.source,
                PublicAccountSource::Derived | PublicAccountSource::HardwareDerived
            )
        })
        .filter(move |account| {
            matches!(
                &account.scope,
                PublicAccountScope::PrivateWallet { wallet_uuid: scoped } if scoped == wallet_uuid
            )
        })
        .filter_map(|account| account.derivation_index)
}

pub(super) fn next_derived_public_account_index(
    metadata: &[PublicAccountMetadata],
    wallet_uuid: &str,
) -> Result<u32, VaultError> {
    derived_public_account_indexes(metadata, wallet_uuid)
        .max()
        .map_or(Ok(0), |max_index| {
            max_index
                .checked_add(1)
                .ok_or(VaultError::PublicAccountDisplayOrderOverflow)
        })
}

pub(super) fn ensure_public_account_address_available(
    metadata: &[PublicAccountMetadata],
    address: Address,
    scope: &PublicAccountScope,
    selected_wallet_uuid: &str,
) -> Result<(), VaultError> {
    let duplicates = match scope {
        PublicAccountScope::Global => metadata.iter().any(|account| {
            account.status == PublicAccountStatus::Active && account.address == address
        }),
        PublicAccountScope::PrivateWallet { .. } => metadata.iter().any(|account| {
            account.address == address && account.is_active_for_wallet(selected_wallet_uuid)
        }),
    };
    if duplicates {
        Err(VaultError::DuplicatePublicAccountAddress)
    } else {
        Ok(())
    }
}

pub(super) fn initial_derived_public_account(
    wallet_uuid: &str,
    entropy: &[u8],
    existing_accounts: &[PublicAccountMetadata],
) -> Result<PublicAccountMetadata, VaultError> {
    let address = derive_public_evm_address_from_entropy(entropy, 0)?;
    let scope = PublicAccountScope::PrivateWallet {
        wallet_uuid: wallet_uuid.to_owned(),
    };
    ensure_public_account_address_available(existing_accounts, address, &scope, wallet_uuid)?;
    Ok(PublicAccountMetadata {
        public_account_uuid: generate_opaque_id()?,
        address,
        label: Some(public_account_default_label(
            next_public_account_label_number(existing_accounts, wallet_uuid),
        )),
        source: PublicAccountSource::Derived,
        scope,
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: next_public_account_display_order(existing_accounts)?,
    })
}

pub(super) fn initial_derived_public_account_from_seed(
    wallet_uuid: &str,
    seed: &[u8; 64],
    existing_accounts: &[PublicAccountMetadata],
) -> Result<PublicAccountMetadata, VaultError> {
    let address = derive_public_evm_address_from_seed(seed, 0)?;
    let scope = PublicAccountScope::PrivateWallet {
        wallet_uuid: wallet_uuid.to_owned(),
    };
    ensure_public_account_address_available(existing_accounts, address, &scope, wallet_uuid)?;
    Ok(PublicAccountMetadata {
        public_account_uuid: generate_opaque_id()?,
        address,
        label: Some(public_account_default_label(
            next_public_account_label_number(existing_accounts, wallet_uuid),
        )),
        source: PublicAccountSource::Derived,
        scope,
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: next_public_account_display_order(existing_accounts)?,
    })
}

pub(super) fn next_public_account_label_number(
    metadata: &[PublicAccountMetadata],
    wallet_uuid: &str,
) -> u32 {
    u32::try_from(
        metadata
            .iter()
            .filter(|account| account.is_scoped_to_wallet(wallet_uuid))
            .count(),
    )
    .ok()
    .and_then(|count| count.checked_add(1))
    .unwrap_or(u32::MAX)
}

pub fn derive_public_evm_private_key_from_entropy(
    entropy: &[u8],
    derivation_index: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    let mnemonic = Zeroizing::new(bip39_mnemonic_from_entropy(entropy)?);
    derive_public_evm_private_key_from_mnemonic_with_passphrase(&mnemonic, "", derivation_index)
}

pub fn derive_public_evm_private_key_from_mnemonic(
    mnemonic: &str,
    derivation_index: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    derive_public_evm_private_key_from_mnemonic_with_passphrase(mnemonic, "", derivation_index)
}

pub fn derive_public_evm_private_key_from_mnemonic_with_passphrase(
    mnemonic: &str,
    passphrase: &str,
    derivation_index: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    let seed = bip39_seed_from_mnemonic(mnemonic, passphrase)?;
    derive_public_evm_private_key_from_seed(&seed, derivation_index)
}

pub(super) fn derive_public_evm_private_key_from_seed(
    seed: &[u8; 64],
    derivation_index: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    let child = derive_public_evm_account_key_from_seed(seed)?
        .derive_child(derivation_index)
        .map_err(|_| VaultError::PublicEvmKeyDerivation)?;
    let signing_key: &coins_bip32::ecdsa::SigningKey = child.as_ref();
    let bytes = signing_key.to_bytes();
    let mut private_key = [0u8; KEY_LEN];
    private_key.copy_from_slice(bytes.as_slice());
    Ok(Zeroizing::new(private_key))
}

/// Extended key at `m/44'/60'/0'/0`, the parent of every derived Public account.
fn derive_public_evm_account_key_from_seed(seed: &[u8; 64]) -> Result<XPriv, VaultError> {
    XPriv::root_from_seed(seed, None)
        .and_then(|root| root.derive_child(coins_bip32::BIP32_HARDEN + 44))
        .and_then(|key| key.derive_child(coins_bip32::BIP32_HARDEN + 60))
        .and_then(|key| key.derive_child(coins_bip32::BIP32_HARDEN))
        .and_then(|key| key.derive_child(0))
        .map_err(|_| VaultError::PublicEvmKeyDerivation)
}

/// Derives Public account addresses for one wallet from the extended public key
/// at `m/44'/60'/0'/0`. It holds no signing material, but it links every derived
/// address, so it deliberately has no `Debug` and should be dropped with its dialog.
pub struct DerivedPublicAccountPreview {
    pub(super) wallet_uuid: String,
    account_key: XPub,
}

impl DerivedPublicAccountPreview {
    pub const PAGE_SIZE: u32 = 20;
    pub const MAX_ADD_INDEXES: usize = 100;
    /// Exclusive bound: higher indexes would be hardened and need the private key.
    pub const INDEX_LIMIT: u32 = coins_bip32::BIP32_HARDEN;

    pub(super) fn from_seed(wallet_uuid: &str, seed: &[u8; 64]) -> Result<Self, VaultError> {
        Ok(Self {
            wallet_uuid: wallet_uuid.to_owned(),
            account_key: derive_public_evm_account_key_from_seed(seed)?.verify_key(),
        })
    }

    pub(super) fn from_entropy(wallet_uuid: &str, entropy: &[u8]) -> Result<Self, VaultError> {
        let mnemonic = Zeroizing::new(bip39_mnemonic_from_entropy(entropy)?);
        let seed = bip39_seed_from_mnemonic(&mnemonic, "")?;
        Self::from_seed(wallet_uuid, &seed)
    }

    pub fn address(&self, derivation_index: u32) -> Result<Address, VaultError> {
        if derivation_index >= Self::INDEX_LIMIT {
            return Err(VaultError::InvalidPublicAccountSelection);
        }
        let child = self
            .account_key
            .derive_child(derivation_index)
            .map_err(|_| VaultError::PublicEvmKeyDerivation)?;
        Ok(Address::from_public_key(child.as_ref()))
    }

    /// Up to `PAGE_SIZE` consecutive indexes from `start`, stopping at `INDEX_LIMIT`.
    pub fn page(&self, start: u32) -> Result<Vec<(u32, Address)>, VaultError> {
        if start >= Self::INDEX_LIMIT {
            return Err(VaultError::InvalidPublicAccountSelection);
        }
        let end = start.saturating_add(Self::PAGE_SIZE).min(Self::INDEX_LIMIT);
        (start..end)
            .map(|index| Ok((index, self.address(index)?)))
            .collect()
    }
}

pub(super) fn derive_public_evm_address_from_seed(
    seed: &[u8; 64],
    derivation_index: u32,
) -> Result<Address, VaultError> {
    let private_key = derive_public_evm_private_key_from_seed(seed, derivation_index)?;
    public_evm_address_from_private_key(&private_key)
}

pub fn derive_public_evm_address_from_mnemonic_with_passphrase(
    mnemonic: &str,
    passphrase: &str,
    derivation_index: u32,
) -> Result<Address, VaultError> {
    let private_key = derive_public_evm_private_key_from_mnemonic_with_passphrase(
        mnemonic,
        passphrase,
        derivation_index,
    )?;
    public_evm_address_from_private_key(&private_key)
}

pub fn derive_public_evm_address_from_entropy(
    entropy: &[u8],
    derivation_index: u32,
) -> Result<Address, VaultError> {
    let private_key = derive_public_evm_private_key_from_entropy(entropy, derivation_index)?;
    public_evm_address_from_private_key(&private_key)
}

pub fn parse_public_evm_private_key(
    private_key: &str,
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    let pk_hex = private_key
        .trim()
        .strip_prefix("0x")
        .unwrap_or_else(|| private_key.trim());
    let bytes =
        alloy::hex::decode_to_array(pk_hex).map_err(|_| VaultError::InvalidPublicEvmPrivateKey)?;
    let private_key = Zeroizing::new(bytes);
    public_evm_address_from_private_key(&private_key)?;
    Ok(private_key)
}

pub fn public_evm_address_from_private_key(
    private_key: &[u8; KEY_LEN],
) -> Result<Address, VaultError> {
    let signing_key = SigningKey::from_bytes(private_key.into())
        .map_err(|_| VaultError::InvalidPublicEvmPrivateKey)?;
    Ok(PrivateKeySigner::from(signing_key).address())
}
