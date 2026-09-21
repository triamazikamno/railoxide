use super::wallet_metadata::wallet_metadata_update_guard;
use super::{
    BTreeSet, DesktopVaultStore, DesktopViewSession, EncryptedRecord, HARDWARE_PROFILE_PREFIX,
    HARDWARE_WALLET_ACCOUNT_INDEX_PREFIX, HardwareDerivationDescriptor, HardwareDeviceKind,
    HardwareProfileBinding, HardwareProfileBindingKind, HardwareProfileMetadata,
    HardwareProfileSession, HardwareRailgunAccountIdentity, HardwareRailgunAccountMetadata,
    HardwareViewAccessKey, HardwareWalletAccountIndexReservation, HardwareWalletProfile,
    MAX_HARDWARE_RECOVERY_RANGE_COUNT, SoftwareRailgunSpendSigner, StoredHardwareWalletRecord,
    VaultError, VaultRecordEntries, ViewUnlock, WalletKeys, WalletMetadataBundle, WalletSource,
    WalletViewBundle, generate_opaque_id, hardware_profile_record_entry,
    hardware_wallet_account_index_record_entry, sort_hardware_profile_metadata, unlock_view,
    wallet_chain_index_complete_record_entry, wallet_metadata_record_entry,
    wallet_metadata_record_key, wallet_view_record_key,
};

fn hardware_wallet_receive_address(wallet: &WalletKeys) -> Result<String, VaultError> {
    wallet
        .viewing
        .derive_address(None)
        .map(|address| address.to_string())
        .map_err(|_| VaultError::HardwareWalletReceiveAddress)
}

impl DesktopVaultStore {
    /// Validate the retained root without requesting spend material or creating allocation state.
    pub(crate) fn validate_executor_source(
        &self,
        view: &DesktopViewSession,
    ) -> Result<(), VaultError> {
        let metadata = self.load_wallet_metadata_for_session(view)?;
        if metadata.derivation_index != view.derivation_index() {
            return Err(VaultError::InvalidWalletMetadata);
        }
        if metadata.source.is_hardware_derived() {
            let account = metadata
                .hardware_account
                .as_ref()
                .ok_or(VaultError::HardwareWalletIdentityMismatch)?;
            Self::ensure_supported_hardware_account(account)?;
            let profile = view
                .hardware_profile_session()
                .ok_or(VaultError::HardwareWalletViewRequiresDevice)?;
            profile.verify_account(account)?;
            if metadata.source
                != WalletSource::from_hardware_device_kind(account.descriptor.device_kind)
                || metadata.hardware_descriptor.as_ref() != Some(&account.descriptor)
                || account.account_index != view.derivation_index()
                || account.account_identity.spending_public_key
                    != view.spending_public_key().map(|key| key.to_be_bytes())
                || account.account_identity.viewing_public_key
                    != view.scan_keys().viewing_public_key
            {
                return Err(VaultError::HardwareWalletIdentityMismatch);
            }
        } else if view.hardware_profile_session().is_some()
            || metadata.hardware_account.is_some()
            || metadata.hardware_descriptor.is_some()
            || metadata.software_context.is_none()
        {
            return Err(VaultError::InvalidWalletMetadata);
        }
        Ok(())
    }

    pub fn store_hardware_derived_wallet_with_metadata(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        wallet: &WalletKeys,
        metadata: &WalletMetadataBundle,
        view_access_key: &HardwareViewAccessKey,
    ) -> Result<StoredHardwareWalletRecord, VaultError> {
        let (stored, records) = self.encrypted_hardware_wallet_records(
            password,
            wallet_id,
            derivation_index,
            wallet,
            metadata,
            view_access_key,
        )?;
        self.db.put_desktop_wallet_vault_records(&records)?;
        Ok(stored)
    }

    pub fn store_hardware_derived_wallet_from_entropy_with_metadata(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        entropy: &[u8],
        metadata: &WalletMetadataBundle,
        view_access_key: &HardwareViewAccessKey,
    ) -> Result<StoredHardwareWalletRecord, VaultError> {
        let wallet = WalletKeys::from_bip39_entropy(entropy, derivation_index)?;
        self.store_hardware_derived_wallet_with_metadata(
            password,
            wallet_id,
            derivation_index,
            &wallet,
            metadata,
            view_access_key,
        )
    }

    pub fn store_hardware_derived_wallet_from_entropy_with_metadata_for_view(
        &self,
        view: &ViewUnlock,
        wallet_id: &str,
        derivation_index: u32,
        entropy: &[u8],
        metadata: &WalletMetadataBundle,
        view_access_key: &HardwareViewAccessKey,
    ) -> Result<StoredHardwareWalletRecord, VaultError> {
        let wallet = WalletKeys::from_bip39_entropy(entropy, derivation_index)?;
        let (stored, records) = self.encrypted_hardware_wallet_records_with_view(
            view,
            wallet_id,
            derivation_index,
            &wallet,
            metadata,
            view_access_key,
        )?;
        self.db.put_desktop_wallet_vault_records(&records)?;
        Ok(stored)
    }

    pub fn load_hardware_view_session(
        &self,
        password: &str,
        hardware_session: &HardwareProfileSession,
        wallet_id: &str,
        view_access_key: &HardwareViewAccessKey,
    ) -> Result<DesktopViewSession, VaultError> {
        let password_view = self.unlock_view(password)?;
        self.load_hardware_view_session_with_view_unlock(
            &password_view,
            hardware_session,
            wallet_id,
            view_access_key,
        )
    }

    pub fn load_hardware_view_session_with_view_unlock(
        &self,
        password_view: &ViewUnlock,
        hardware_session: &HardwareProfileSession,
        wallet_id: &str,
        view_access_key: &HardwareViewAccessKey,
    ) -> Result<DesktopViewSession, VaultError> {
        let metadata = self.load_wallet_metadata_with_view(password_view, wallet_id)?;
        {
            let account = metadata
                .hardware_account
                .as_ref()
                .ok_or(VaultError::HardwareWalletIdentityMismatch)?;
            Self::ensure_supported_hardware_account(account)?;
            hardware_session.verify_account(account)?;
        }
        let hardware_view = ViewUnlock::from_hardware_view_access_key(view_access_key)?;
        let record = self.encrypted_record(&wallet_view_record_key(wallet_id))?;
        let bundle = hardware_view.decrypt_view_bundle(wallet_id, &record)?;
        let account = metadata
            .hardware_account
            .as_ref()
            .ok_or(VaultError::HardwareWalletIdentityMismatch)?;
        if HardwareRailgunAccountIdentity::from_view_bundle(&bundle) != account.account_identity {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        }
        let session = DesktopViewSession::from_hardware_bundle(
            wallet_id.to_owned(),
            &bundle,
            password_view.clone_unlock(),
            hardware_view,
            hardware_session.clone(),
        );
        let receive_address = session
            .receive_address()
            .map_err(|_| VaultError::HardwareWalletReceiveAddress)?;
        let needs_receive_address_update =
            metadata.hardware_account.as_ref().is_some_and(|account| {
                account.receive_address.as_deref() != Some(receive_address.as_str())
            });
        if needs_receive_address_update {
            let _guard = wallet_metadata_update_guard();
            let mut current = self.load_wallet_metadata_with_view(password_view, wallet_id)?;
            let account = current
                .hardware_account
                .as_mut()
                .ok_or(VaultError::HardwareWalletIdentityMismatch)?;
            hardware_session.verify_account(account)?;
            if account.receive_address.as_deref() != Some(receive_address.as_str()) {
                account.receive_address = Some(receive_address);
                self.store_wallet_metadata_with_view(password_view, &current)?;
            }
        }
        Ok(session)
    }

    pub fn hardware_railgun_spend_signer_from_entropy(
        &self,
        view_session: &DesktopViewSession,
        descriptor: &HardwareDerivationDescriptor,
        entropy: &[u8],
    ) -> Result<SoftwareRailgunSpendSigner, VaultError> {
        self.hardware_seed_and_signer_for_session(view_session, descriptor, entropy)
            .map(|(_seed, signer)| signer)
    }

    /// Reconstruct the existing synthetic BIP-39 root after checking its custody
    /// and identity. Native authorization owns the seed until account selection.
    pub(crate) fn hardware_seed_and_signer_for_session(
        &self,
        view_session: &DesktopViewSession,
        descriptor: &HardwareDerivationDescriptor,
        entropy: &[u8],
    ) -> Result<(zeroize::Zeroizing<[u8; 64]>, SoftwareRailgunSpendSigner), VaultError> {
        descriptor
            .validate()
            .map_err(|_| VaultError::InvalidHardwareWalletDescriptor)?;
        let metadata =
            self.load_wallet_metadata_with_view(&view_session.view, view_session.wallet_id())?;
        let account = metadata
            .hardware_account
            .as_ref()
            .ok_or(VaultError::HardwareWalletIdentityMismatch)?;
        Self::ensure_supported_hardware_account(account)?;
        let profile = view_session
            .hardware_profile_session()
            .ok_or(VaultError::HardwareWalletIdentityMismatch)?;
        profile.verify_account(account)?;
        if metadata.source != WalletSource::from_hardware_device_kind(descriptor.device_kind)
            || metadata.hardware_descriptor.as_ref() != Some(descriptor)
            || account.descriptor != *descriptor
            || descriptor.account_index != view_session.derivation_index()
        {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        }
        // The hardware passphrase is already part of the device context.
        let mnemonic =
            zeroize::Zeroizing::new(railgun_wallet::bip39_mnemonic_from_entropy(entropy)?);
        let seed = crate::vault::bip39_seed_from_mnemonic(&mnemonic, "")?;
        let signer = SoftwareRailgunSpendSigner {
            wallet: WalletKeys::from_seed(&seed, descriptor.account_index)?,
        };
        if signer.wallet.spending_public_key != view_session.spending_public_key()
            || HardwareRailgunAccountIdentity::from_wallet_keys(&signer.wallet)
                != account.account_identity
        {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        }
        Ok((seed, signer))
    }

    pub fn list_hardware_wallet_profiles(
        &self,
        password: &str,
    ) -> Result<Vec<HardwareWalletProfile>, VaultError> {
        let view = self.unlock_view(password)?;
        self.list_hardware_wallet_profiles_with_view(&view)
    }

    pub fn list_hardware_wallet_profiles_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<HardwareWalletProfile>, VaultError> {
        self.list_hardware_wallet_profiles_with_view(&view_session.view)
    }

    pub fn list_hardware_profile_metadata(
        &self,
        password: &str,
    ) -> Result<Vec<HardwareProfileMetadata>, VaultError> {
        let view = self.unlock_view(password)?;
        self.list_hardware_profile_metadata_with_view(&view)
    }

    pub fn list_hardware_profile_metadata_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<HardwareProfileMetadata>, VaultError> {
        self.list_hardware_profile_metadata_with_view(&view_session.view)
    }

    pub fn list_hardware_profile_metadata_with_view_unlock(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<HardwareProfileMetadata>, VaultError> {
        self.list_hardware_profile_metadata_with_view(view)
    }

    pub fn store_hardware_profile_metadata(
        &self,
        password: &str,
        profile: &HardwareProfileMetadata,
    ) -> Result<(), VaultError> {
        let view = self.unlock_view(password)?;
        self.store_hardware_profile_metadata_with_view(&view, profile)
    }

    pub fn store_hardware_profile_metadata_with_view_unlock(
        &self,
        view: &ViewUnlock,
        profile: &HardwareProfileMetadata,
    ) -> Result<(), VaultError> {
        self.store_hardware_profile_metadata_with_view(view, profile)
    }

    pub fn list_hardware_accounts_for_profile(
        &self,
        password: &str,
        profile_id: &str,
    ) -> Result<Vec<HardwareRailgunAccountMetadata>, VaultError> {
        let view = self.unlock_view(password)?;
        self.list_hardware_accounts_for_profile_with_view(&view, profile_id)
    }

    pub fn list_hardware_accounts_for_profile_for_session(
        &self,
        view_session: &DesktopViewSession,
        profile_id: &str,
    ) -> Result<Vec<HardwareRailgunAccountMetadata>, VaultError> {
        self.list_hardware_accounts_for_profile_with_view(&view_session.view, profile_id)
    }

    pub fn hardware_profile_session_for_fingerprint(
        &self,
        password: &str,
        device_kind: HardwareDeviceKind,
        profile_fingerprint: &str,
        trezor_session_id: Option<&[u8]>,
    ) -> Result<HardwareProfileSession, VaultError> {
        let view = self.unlock_view(password)?;
        self.hardware_profile_session_for_fingerprint_with_view(
            &view,
            device_kind,
            profile_fingerprint,
            trezor_session_id,
        )
    }

    pub fn hardware_profile_session_for_fingerprint_for_session(
        &self,
        view_session: &DesktopViewSession,
        device_kind: HardwareDeviceKind,
        profile_fingerprint: &str,
        trezor_session_id: Option<&[u8]>,
    ) -> Result<HardwareProfileSession, VaultError> {
        self.hardware_profile_session_for_fingerprint_with_view(
            &view_session.view,
            device_kind,
            profile_fingerprint,
            trezor_session_id,
        )
    }

    pub fn hardware_profile_session_for_fingerprint_with_view_unlock(
        &self,
        view: &ViewUnlock,
        device_kind: HardwareDeviceKind,
        profile_fingerprint: &str,
        trezor_session_id: Option<&[u8]>,
    ) -> Result<HardwareProfileSession, VaultError> {
        self.hardware_profile_session_for_fingerprint_with_view(
            view,
            device_kind,
            profile_fingerprint,
            trezor_session_id,
        )
    }

    pub fn verify_hardware_profile_session_for_account(
        session: &HardwareProfileSession,
        account: &HardwareRailgunAccountMetadata,
    ) -> Result<(), VaultError> {
        session.verify_account(account)
    }

    pub fn next_hardware_account_index_for_profile(
        &self,
        password: &str,
        profile: &HardwareWalletProfile,
    ) -> Result<u32, VaultError> {
        let view = self.unlock_view(password)?;
        self.next_hardware_account_index_for_profile_with_view(&view, profile)
    }

    pub fn next_hardware_account_index_for_profile_for_session(
        &self,
        view_session: &DesktopViewSession,
        profile: &HardwareWalletProfile,
    ) -> Result<u32, VaultError> {
        self.next_hardware_account_index_for_profile_with_view(&view_session.view, profile)
    }

    pub fn next_hardware_account_index_for_profile_with_view_unlock(
        &self,
        view: &ViewUnlock,
        profile: &HardwareWalletProfile,
    ) -> Result<u32, VaultError> {
        self.next_hardware_account_index_for_profile_with_view(view, profile)
    }

    #[must_use]
    pub const fn default_hardware_recovery_account_index() -> u32 {
        0
    }

    pub const fn exact_hardware_recovery_account_index(
        account_index: u32,
    ) -> Result<u32, VaultError> {
        if account_index >= crate::hardware::HARDENED_BIP32_INDEX {
            Err(VaultError::InvalidHardwareAccountRecoveryRange)
        } else {
            Ok(account_index)
        }
    }

    pub fn bounded_hardware_recovery_account_indices(
        start_index: u32,
        count: u32,
    ) -> Result<Vec<u32>, VaultError> {
        if count == 0 || count > MAX_HARDWARE_RECOVERY_RANGE_COUNT {
            return Err(VaultError::InvalidHardwareAccountRecoveryRange);
        }
        let end_exclusive = start_index
            .checked_add(count)
            .ok_or(VaultError::InvalidHardwareAccountRecoveryRange)?;
        if end_exclusive > crate::hardware::HARDENED_BIP32_INDEX {
            return Err(VaultError::InvalidHardwareAccountRecoveryRange);
        }
        Ok((start_index..end_exclusive).collect())
    }

    fn list_hardware_wallet_profiles_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<HardwareWalletProfile>, VaultError> {
        let mut profiles =
            self.list_hardware_profile_metadata_with_view(view)?
                .into_iter()
                .flat_map(|profile| {
                    let device_kind = profile.device_kind;
                    profile.bindings.into_iter().filter_map(move |binding| {
                        (binding.kind == HardwareProfileBindingKind::EvmAddressFingerprint)
                            .then_some(HardwareWalletProfile {
                                device_kind,
                                profile_fingerprint: binding.fingerprint,
                            })
                    })
                })
                .collect::<BTreeSet<_>>();
        let metadata = self.list_wallet_metadata_with_view(view)?;
        profiles.extend(
            metadata
                .into_iter()
                .filter_map(|metadata| metadata.hardware_derivation_descriptor().cloned())
                .map(|descriptor| HardwareWalletProfile {
                    device_kind: descriptor.device_kind,
                    profile_fingerprint: descriptor.profile_fingerprint,
                }),
        );
        Ok(profiles.into_iter().collect())
    }

    fn list_hardware_profile_metadata_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<HardwareProfileMetadata>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(HARDWARE_PROFILE_PREFIX)?;
        let mut profiles = Vec::with_capacity(records.len());
        for stored in records {
            let Some(profile_id) = stored.key.strip_prefix(HARDWARE_PROFILE_PREFIX) else {
                continue;
            };
            let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
            profiles.push(view.decrypt_hardware_profile_metadata(profile_id, &record)?);
        }

        let mut known_ids = profiles
            .iter()
            .map(|profile| profile.profile_id.clone())
            .collect::<BTreeSet<_>>();
        for metadata in self.list_wallet_metadata_with_view(view)? {
            let Some(descriptor) = metadata.hardware_derivation_descriptor() else {
                continue;
            };
            let profile = HardwareProfileMetadata::from_descriptor(descriptor);
            if known_ids.insert(profile.profile_id.clone()) {
                profiles.push(profile);
            }
        }
        sort_hardware_profile_metadata(&mut profiles);
        Ok(profiles)
    }

    fn hardware_profile_session_for_fingerprint_with_view(
        &self,
        view: &ViewUnlock,
        device_kind: HardwareDeviceKind,
        profile_fingerprint: &str,
        trezor_session_id: Option<&[u8]>,
    ) -> Result<HardwareProfileSession, VaultError> {
        let binding = HardwareProfileBinding::evm_address_fingerprint(profile_fingerprint);
        let session = self
            .list_hardware_profile_metadata_with_view(view)?
            .into_iter()
            .find(|profile| {
                profile.device_kind == device_kind
                    && profile
                        .bindings
                        .iter()
                        .any(|candidate| candidate == &binding)
            })
            .map_or_else(
                || {
                    HardwareProfileSession::unmatched(
                        device_kind,
                        binding.clone(),
                        trezor_session_id.map(<[u8]>::to_vec),
                    )
                },
                |profile| {
                    HardwareProfileSession::matched(
                        device_kind,
                        profile.profile_id,
                        binding.clone(),
                        trezor_session_id.map(<[u8]>::to_vec),
                    )
                },
            );
        Ok(session)
    }

    fn store_hardware_profile_metadata_with_view(
        &self,
        view: &ViewUnlock,
        profile: &HardwareProfileMetadata,
    ) -> Result<(), VaultError> {
        let record = hardware_profile_record_entry(view, profile)?;
        self.db.put_desktop_wallet_vault_records(&[record])?;
        Ok(())
    }

    fn list_hardware_accounts_for_profile_with_view(
        &self,
        view: &ViewUnlock,
        profile_id: &str,
    ) -> Result<Vec<HardwareRailgunAccountMetadata>, VaultError> {
        let mut accounts = self
            .list_wallet_metadata_with_view(view)?
            .into_iter()
            .filter_map(|metadata| metadata.hardware_account)
            .filter(|account| account.profile_id == profile_id)
            .collect::<Vec<_>>();
        accounts.sort_by(|left, right| {
            left.account_index
                .cmp(&right.account_index)
                .then_with(|| left.label.cmp(&right.label))
        });
        Ok(accounts)
    }

    fn next_hardware_account_index_for_profile_with_view(
        &self,
        view: &ViewUnlock,
        profile: &HardwareWalletProfile,
    ) -> Result<u32, VaultError> {
        let metadata = self.list_wallet_metadata_with_view(view)?;
        let reserved = self.list_hardware_wallet_account_index_reservations_with_view(view)?;
        metadata
            .into_iter()
            .filter_map(|metadata| metadata.hardware_derivation_descriptor().cloned())
            .filter(|descriptor| {
                descriptor.device_kind == profile.device_kind
                    && descriptor.profile_fingerprint == profile.profile_fingerprint
            })
            .map(|descriptor| descriptor.account_index)
            .chain(
                reserved
                    .into_iter()
                    .filter(|reservation| reservation.profile == *profile)
                    .map(|reservation| reservation.account_index),
            )
            .max()
            .map_or(Ok(0), |index| {
                index
                    .checked_add(1)
                    .ok_or(VaultError::HardwareWalletAccountIndexOverflow)
            })
    }

    fn list_hardware_wallet_account_index_reservations_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<HardwareWalletAccountIndexReservation>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(HARDWARE_WALLET_ACCOUNT_INDEX_PREFIX)?;
        let mut reservations = Vec::with_capacity(records.len());
        for stored in records {
            let Some(reservation_uuid) = stored
                .key
                .strip_prefix(HARDWARE_WALLET_ACCOUNT_INDEX_PREFIX)
            else {
                continue;
            };
            let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
            reservations.push(
                view.decrypt_hardware_wallet_account_index_reservation(reservation_uuid, &record)?,
            );
        }
        Ok(reservations)
    }

    pub(super) fn hardware_wallet_account_index_reservation(
        descriptor: &HardwareDerivationDescriptor,
    ) -> HardwareWalletAccountIndexReservation {
        HardwareWalletAccountIndexReservation {
            profile: HardwareWalletProfile {
                device_kind: descriptor.device_kind,
                profile_fingerprint: descriptor.profile_fingerprint.clone(),
            },
            account_index: descriptor.account_index,
        }
    }

    fn hardware_profile_metadata_for_descriptor_with_view(
        &self,
        view: &ViewUnlock,
        descriptor: &HardwareDerivationDescriptor,
    ) -> Result<HardwareProfileMetadata, VaultError> {
        let wallet_profile = HardwareWalletProfile {
            device_kind: descriptor.device_kind,
            profile_fingerprint: descriptor.profile_fingerprint.clone(),
        };
        let existing = self.list_hardware_profile_metadata_with_view(view)?;
        Ok(existing
            .into_iter()
            .find(|profile| profile.matches_wallet_profile(&wallet_profile))
            .unwrap_or_else(|| HardwareProfileMetadata::from_descriptor(descriptor)))
    }

    pub(super) fn ensure_hardware_wallet_account_index_available(
        metadata: &[WalletMetadataBundle],
        descriptor: &HardwareDerivationDescriptor,
    ) -> Result<(), VaultError> {
        let duplicate = metadata
            .iter()
            .filter_map(WalletMetadataBundle::hardware_derivation_descriptor)
            .any(|existing| {
                existing.device_kind == descriptor.device_kind
                    && existing.profile_fingerprint == descriptor.profile_fingerprint
                    && existing.account_index == descriptor.account_index
            });
        if duplicate {
            Err(VaultError::DuplicateHardwareWalletAccountIndex)
        } else {
            Ok(())
        }
    }

    fn encrypted_hardware_wallet_records(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        wallet: &WalletKeys,
        metadata: &WalletMetadataBundle,
        view_access_key: &HardwareViewAccessKey,
    ) -> Result<(StoredHardwareWalletRecord, VaultRecordEntries), VaultError> {
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        self.encrypted_hardware_wallet_records_with_view(
            &view,
            wallet_id,
            derivation_index,
            wallet,
            metadata,
            view_access_key,
        )
    }

    fn encrypted_hardware_wallet_records_with_view(
        &self,
        view: &ViewUnlock,
        wallet_id: &str,
        derivation_index: u32,
        wallet: &WalletKeys,
        metadata: &WalletMetadataBundle,
        view_access_key: &HardwareViewAccessKey,
    ) -> Result<(StoredHardwareWalletRecord, VaultRecordEntries), VaultError> {
        let descriptor = metadata
            .hardware_descriptor
            .as_ref()
            .ok_or(VaultError::InvalidHardwareWalletDescriptor)?;
        descriptor
            .validate()
            .map_err(|_| VaultError::InvalidHardwareWalletDescriptor)?;
        if !metadata.source.is_hardware_derived()
            || metadata.source != WalletSource::from_hardware_device_kind(descriptor.device_kind)
            || metadata.wallet_uuid != wallet_id
            || metadata.derivation_index != derivation_index
            || descriptor.account_index != derivation_index
        {
            return Err(VaultError::InvalidHardwareWalletDescriptor);
        }

        let existing = self.list_wallet_metadata_with_view(view)?;
        Self::ensure_hardware_wallet_account_index_available(&existing, descriptor)?;
        let profile = self.hardware_profile_metadata_for_descriptor_with_view(view, descriptor)?;
        let hardware_view = ViewUnlock::from_hardware_view_access_key(view_access_key)?;
        let view_bundle = WalletViewBundle::from_wallet_keys(derivation_index, wallet);
        let receive_address = hardware_wallet_receive_address(wallet)?;
        let view_record = hardware_view.encrypt_view_bundle(wallet_id, &view_bundle)?;
        let view_record_key = wallet_view_record_key(wallet_id);
        let mut stored_metadata = metadata.clone();
        stored_metadata.hardware_account = Some(
            HardwareRailgunAccountMetadata::synthetic_software_v1(
                profile.profile_id.clone(),
                descriptor.account_index,
                metadata.label.clone(),
                descriptor.clone(),
                HardwareRailgunAccountIdentity::from_wallet_keys(wallet),
            )
            .with_receive_address(receive_address),
        );
        let metadata_record_key = wallet_metadata_record_key(&stored_metadata.wallet_uuid);
        let reservation = Self::hardware_wallet_account_index_reservation(descriptor);
        let records = vec![
            view_record.to_record_entry(view_record_key.clone())?,
            wallet_metadata_record_entry(view, &stored_metadata)?,
            wallet_chain_index_complete_record_entry(&stored_metadata.wallet_uuid)?,
            hardware_profile_record_entry(view, &profile)?,
            hardware_wallet_account_index_record_entry(view, &generate_opaque_id()?, &reservation)?,
        ];

        Ok((
            StoredHardwareWalletRecord {
                wallet_id: wallet_id.to_owned(),
                derivation_index,
                view_record_key,
                metadata_record_key,
            },
            records,
        ))
    }
}
