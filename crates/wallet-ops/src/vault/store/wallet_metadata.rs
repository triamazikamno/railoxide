use local_db::WalletDeletionBatch;

static WALLET_METADATA_UPDATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn wallet_metadata_update_guard() -> std::sync::MutexGuard<'static, ()> {
    WALLET_METADATA_UPDATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

use super::{
    BTreeSet, DesktopVaultStore, DesktopViewSession, EncryptedRecord, HardwareDerivationDescriptor,
    HardwareRailgunAccountMetadata, HardwareWalletSyncIntent, LoadedWalletMetadata,
    PublicAccountScope, StoredWalletContextRecord, VaultError, ViewUnlock,
    WALLET_CHAIN_INDEX_COMPLETE_VERSION, WALLET_CHAIN_INDEX_PREFIX, WALLET_CHAIN_METADATA_PREFIX,
    WalletCacheKey, WalletMetadataBundle, WalletPrivateNamespaceId, WalletSoftwareContext,
    WalletSoftwareContextKind, WalletSource, WalletSpendSource, WalletStatus, WalletViewBundle,
    assign_missing_display_orders, default_wallet_label_for_metadata, generate_opaque_id,
    hardware_wallet_account_index_record_entry, next_wallet_display_order,
    public_account_metadata_record_key, public_account_secret_record_key, sort_wallet_metadata,
    validate_wallet_label, wallet_chain_index_complete_record_key, wallet_chain_index_prefix,
    wallet_chain_index_record_key, wallet_chain_metadata_record_key, wallet_metadata_record_entry,
    wallet_metadata_record_key, wallet_spend_record_key, wallet_view_record_key,
};

impl DesktopVaultStore {
    pub fn new_passphrase_context_metadata(
        &self,
        password: &str,
        base_profile_uuid: &str,
        wallet_uuid: &str,
        derivation_index: u32,
        label: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let view = self.unlock_view(password)?;
        self.new_passphrase_context_metadata_with_view_unlock(
            &view,
            base_profile_uuid,
            wallet_uuid,
            derivation_index,
            label,
        )
    }

    pub fn new_passphrase_context_metadata_with_view_unlock(
        &self,
        view: &ViewUnlock,
        base_profile_uuid: &str,
        wallet_uuid: &str,
        derivation_index: u32,
        label: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let existing = self.list_wallet_metadata_with_view(view)?;
        let base = standard_base_profile(&existing, base_profile_uuid)?;
        if existing
            .iter()
            .any(|metadata| metadata.wallet_uuid == wallet_uuid)
        {
            return Err(VaultError::InvalidWalletMetadata);
        }
        let label = validate_wallet_label(label, &existing, None)?;
        let display_order = next_wallet_display_order(&existing)?;
        let metadata = WalletMetadataBundle {
            wallet_uuid: wallet_uuid.to_owned(),
            label,
            derivation_index,
            source: base.source,
            status: WalletStatus::Active,
            display_order,
            hardware_descriptor: None,
            hardware_account: None,
            pending_create_new_chain_ids: BTreeSet::new(),
            software_context: Some(WalletSoftwareContext::passphrase(base_profile_uuid)),
        };
        metadata.validate()?;
        Ok(metadata)
    }

    pub fn store_passphrase_context_with_view_bundle(
        &self,
        password: &str,
        base_profile_uuid: &str,
        wallet_uuid: &str,
        view_bundle: &WalletViewBundle,
        metadata: &WalletMetadataBundle,
    ) -> Result<StoredWalletContextRecord, VaultError> {
        let view = self.unlock_view(password)?;
        self.store_passphrase_context_with_view_bundle_with_view_unlock(
            &view,
            base_profile_uuid,
            wallet_uuid,
            view_bundle,
            metadata,
        )
    }

    pub fn store_passphrase_context_with_view_bundle_with_view_unlock(
        &self,
        view: &ViewUnlock,
        base_profile_uuid: &str,
        wallet_uuid: &str,
        view_bundle: &WalletViewBundle,
        metadata: &WalletMetadataBundle,
    ) -> Result<StoredWalletContextRecord, VaultError> {
        let existing = self.list_wallet_metadata_with_view(view)?;
        standard_base_profile(&existing, base_profile_uuid)?;
        if existing
            .iter()
            .any(|existing| existing.wallet_uuid == wallet_uuid)
        {
            return Err(VaultError::InvalidWalletMetadata);
        }
        let Some(context) = metadata.software_context.as_ref() else {
            return Err(VaultError::InvalidWalletMetadata);
        };
        if metadata.wallet_uuid != wallet_uuid
            || metadata.derivation_index != view_bundle.derivation_index
            || metadata.source.is_hardware_derived()
            || context.kind != WalletSoftwareContextKind::Passphrase
            || context.base_profile_uuid != base_profile_uuid
        {
            return Err(VaultError::InvalidWalletMetadata);
        }
        metadata.validate()?;

        let view_record = view.encrypt_view_bundle(wallet_uuid, view_bundle)?;
        let metadata_record = view.encrypt_wallet_metadata(wallet_uuid, metadata)?;
        let view_record_key = wallet_view_record_key(wallet_uuid);
        let metadata_record_key = wallet_metadata_record_key(wallet_uuid);
        self.db.put_desktop_wallet_vault_records(&[
            view_record.to_record_entry(view_record_key.clone())?,
            metadata_record.to_record_entry(metadata_record_key.clone())?,
        ])?;
        Ok(StoredWalletContextRecord {
            wallet_id: wallet_uuid.to_owned(),
            derivation_index: view_bundle.derivation_index,
            view_record_key,
            metadata_record_key,
        })
    }

    pub fn store_wallet_metadata(
        &self,
        password: &str,
        metadata: &WalletMetadataBundle,
    ) -> Result<(), VaultError> {
        let view = self.unlock_view(password)?;
        self.store_wallet_metadata_with_view(&view, metadata)
    }

    pub(super) fn store_wallet_metadata_with_view(
        &self,
        view: &ViewUnlock,
        metadata: &WalletMetadataBundle,
    ) -> Result<(), VaultError> {
        let (key, data) = wallet_metadata_record_entry(view, metadata)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(())
    }

    pub(super) fn store_wallet_metadata_batch_with_view(
        &self,
        view: &ViewUnlock,
        metadata: &[WalletMetadataBundle],
    ) -> Result<(), VaultError> {
        let records = metadata
            .iter()
            .map(|metadata| wallet_metadata_record_entry(view, metadata))
            .collect::<Result<Vec<_>, _>>()?;
        self.db.put_desktop_wallet_vault_records(&records)?;
        Ok(())
    }

    pub fn load_wallet_metadata(
        &self,
        password: &str,
        wallet_uuid: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let view = self.unlock_view(password)?;
        self.load_wallet_metadata_with_view(&view, wallet_uuid)
    }

    pub fn load_wallet_metadata_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.load_wallet_metadata_with_view(&view_session.view, view_session.wallet_id())
    }

    pub(super) fn load_wallet_metadata_with_view(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let record = self.encrypted_record(&wallet_metadata_record_key(wallet_uuid))?;
        view.decrypt_wallet_metadata(wallet_uuid, &record)
    }

    pub(super) fn load_wallet_metadata_optional_with_view(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
    ) -> Result<Option<WalletMetadataBundle>, VaultError> {
        let Some(record) =
            self.encrypted_record_optional(&wallet_metadata_record_key(wallet_uuid))?
        else {
            return Ok(None);
        };
        view.decrypt_wallet_metadata(wallet_uuid, &record).map(Some)
    }

    pub(super) fn ensure_password_view_allowed(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
    ) -> Result<(), VaultError> {
        let Some(metadata) = self.load_wallet_metadata_optional_with_view(view, wallet_uuid)?
        else {
            return Ok(());
        };
        if let Some(account) = metadata.hardware_account {
            if account.custody_backend.is_supported() {
                Err(VaultError::HardwareWalletViewRequiresDevice)
            } else {
                Err(VaultError::UnsupportedHardwareCustodyBackend(
                    account.custody_backend.as_str().to_owned(),
                ))
            }
        } else if metadata.hardware_descriptor.is_some() {
            Err(VaultError::HardwareWalletViewRequiresDevice)
        } else {
            Ok(())
        }
    }

    pub(super) fn ensure_supported_hardware_account(
        account: &HardwareRailgunAccountMetadata,
    ) -> Result<(), VaultError> {
        if account.custody_backend.is_supported() {
            Ok(())
        } else {
            Err(VaultError::UnsupportedHardwareCustodyBackend(
                account.custody_backend.as_str().to_owned(),
            ))
        }
    }

    pub fn list_wallet_metadata(
        &self,
        password: &str,
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let view = self.unlock_view(password)?;
        self.list_wallet_metadata_with_view(&view)
    }

    pub fn list_wallet_metadata_with_view_unlock(
        &self,
        view: &ViewUnlock,
        include_inactive: bool,
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let mut metadata = self.list_wallet_metadata_with_view(view)?;
        if !include_inactive {
            metadata.retain(|metadata| metadata.status == WalletStatus::Active);
        }
        sort_wallet_metadata(&mut metadata);
        Ok(metadata)
    }

    pub fn list_wallet_metadata_for_session(
        &self,
        view_session: &DesktopViewSession,
        include_inactive: bool,
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let mut metadata = self.list_wallet_metadata_with_view(&view_session.view)?;
        if !include_inactive {
            metadata.retain(|metadata| metadata.status == WalletStatus::Active);
        }
        sort_wallet_metadata(&mut metadata);
        Ok(metadata)
    }

    pub fn wallet_spend_source_for_session(
        &self,
        view_session: &DesktopViewSession,
        wallet_uuid: &str,
    ) -> Result<WalletSpendSource, VaultError> {
        let metadata = self.load_wallet_metadata_with_view(&view_session.view, wallet_uuid)?;
        if let Some(account) = metadata.hardware_account {
            Self::ensure_supported_hardware_account(&account)?;
            return Ok(WalletSpendSource::HardwareDerived(account.descriptor));
        }
        if let Some(descriptor) = metadata.hardware_descriptor {
            return Ok(WalletSpendSource::HardwareDerived(descriptor));
        }
        Ok(WalletSpendSource::Software)
    }

    pub(super) fn list_wallet_metadata_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let _guard = wallet_metadata_update_guard();
        self.list_wallet_metadata_with_view_unlocked(view)
    }

    fn list_wallet_metadata_with_view_unlocked(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let mut wallet_ids = self.list_wallet_ids()?;
        wallet_ids.sort();

        let mut loaded = Vec::with_capacity(wallet_ids.len());
        let mut missing_wallets = Vec::new();
        for wallet_id in wallet_ids {
            let Some(record) =
                self.encrypted_record_optional(&wallet_metadata_record_key(&wallet_id))?
            else {
                let view_record = self.encrypted_record(&wallet_view_record_key(&wallet_id))?;
                let view_bundle = view.decrypt_view_bundle(&wallet_id, &view_record)?;
                missing_wallets.push((wallet_id, view_bundle.derivation_index));
                continue;
            };

            let mut decoded = view.decrypt_wallet_metadata_record(&wallet_id, &record)?;
            if decoded.metadata.wallet_uuid != wallet_id {
                decoded.metadata.wallet_uuid.clone_from(&wallet_id);
                decoded.missing_lifecycle_fields = true;
            }
            if decoded.missing_software_context {
                decoded.metadata.software_context =
                    Some(WalletSoftwareContext::standard(wallet_id.clone()));
            }
            loaded.push(LoadedWalletMetadata {
                metadata: decoded.metadata,
                needs_persist: decoded.missing_lifecycle_fields || decoded.missing_software_context,
                missing_display_order: decoded.missing_display_order,
            });
        }

        for (wallet_id, derivation_index) in missing_wallets {
            let existing = loaded
                .iter()
                .map(|loaded| loaded.metadata.clone())
                .collect::<Vec<_>>();
            let label = default_wallet_label_for_metadata(&existing);
            loaded.push(LoadedWalletMetadata {
                metadata: WalletMetadataBundle {
                    wallet_uuid: wallet_id.clone(),
                    label,
                    derivation_index,
                    source: WalletSource::Imported,
                    status: WalletStatus::Active,
                    display_order: 0,
                    hardware_descriptor: None,
                    hardware_account: None,
                    pending_create_new_chain_ids: BTreeSet::new(),
                    software_context: Some(WalletSoftwareContext::standard(wallet_id)),
                },
                needs_persist: true,
                missing_display_order: true,
            });
        }

        assign_missing_display_orders(&mut loaded)?;
        if loaded.iter().any(|loaded| loaded.needs_persist) {
            let mut records = Vec::new();
            for loaded in loaded.iter().filter(|loaded| loaded.needs_persist) {
                records.push(wallet_metadata_record_entry(view, &loaded.metadata)?);
            }
            self.db.put_desktop_wallet_vault_records(&records)?;
        }

        let mut metadata = loaded
            .into_iter()
            .map(|loaded| loaded.metadata)
            .collect::<Vec<_>>();
        sort_wallet_metadata(&mut metadata);
        Ok(metadata)
    }

    pub fn active_wallet_metadata(
        &self,
        password: &str,
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let mut metadata = self.list_wallet_metadata(password)?;
        metadata.retain(|metadata| metadata.status == WalletStatus::Active);
        sort_wallet_metadata(&mut metadata);
        Ok(metadata)
    }

    pub fn default_wallet_label(&self, password: &str) -> Result<String, VaultError> {
        let metadata = self.list_wallet_metadata(password)?;
        Ok(default_wallet_label_for_metadata(&metadata))
    }

    pub fn preflight_new_wallet_metadata(
        &self,
        password: &str,
        label: &str,
    ) -> Result<String, VaultError> {
        self.new_wallet_label_and_order(password, label)
            .map(|(label, _display_order)| label)
    }

    fn new_wallet_label_and_order(
        &self,
        password: &str,
        label: &str,
    ) -> Result<(String, u32), VaultError> {
        let existing = self.list_wallet_metadata(password)?;
        let label = validate_wallet_label(label, &existing, None)?;
        let display_order = next_wallet_display_order(&existing)?;
        Ok((label, display_order))
    }

    pub fn new_wallet_metadata(
        &self,
        password: &str,
        wallet_uuid: &str,
        derivation_index: u32,
        source: WalletSource,
        label: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.new_wallet_metadata_with_pending_create_new_chain_ids(
            password,
            wallet_uuid,
            derivation_index,
            source,
            label,
            BTreeSet::new(),
        )
    }

    pub fn new_wallet_metadata_with_pending_create_new_chain_ids(
        &self,
        password: &str,
        wallet_uuid: &str,
        derivation_index: u32,
        source: WalletSource,
        label: &str,
        pending_create_new_chain_ids: BTreeSet<u64>,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let (label, display_order) = self.new_wallet_label_and_order(password, label)?;
        let pending_create_new_chain_ids = if source == WalletSource::Generated {
            pending_create_new_chain_ids
        } else {
            BTreeSet::new()
        };
        Ok(WalletMetadataBundle {
            wallet_uuid: wallet_uuid.to_owned(),
            label,
            derivation_index,
            source,
            status: WalletStatus::Active,
            display_order,
            hardware_descriptor: None,
            hardware_account: None,
            pending_create_new_chain_ids,
            software_context: Some(WalletSoftwareContext::standard(wallet_uuid)),
        })
    }

    pub fn new_hardware_wallet_metadata(
        &self,
        password: &str,
        wallet_uuid: &str,
        label: &str,
        descriptor: HardwareDerivationDescriptor,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.new_hardware_wallet_metadata_with_pending_create_new_chain_ids(
            password,
            wallet_uuid,
            label,
            descriptor,
            BTreeSet::new(),
        )
    }

    pub fn new_hardware_wallet_metadata_with_pending_create_new_chain_ids(
        &self,
        password: &str,
        wallet_uuid: &str,
        label: &str,
        descriptor: HardwareDerivationDescriptor,
        pending_create_new_chain_ids: BTreeSet<u64>,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let view = self.unlock_view(password)?;
        self.new_hardware_wallet_metadata_with_view(
            &view,
            wallet_uuid,
            label,
            descriptor,
            pending_create_new_chain_ids,
        )
    }

    pub fn new_hardware_wallet_metadata_with_view_unlock(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
        label: &str,
        descriptor: HardwareDerivationDescriptor,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.new_hardware_wallet_metadata_with_pending_create_new_chain_ids_for_view_unlock(
            view,
            wallet_uuid,
            label,
            descriptor,
            BTreeSet::new(),
        )
    }

    pub fn new_hardware_wallet_metadata_with_pending_create_new_chain_ids_for_view_unlock(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
        label: &str,
        descriptor: HardwareDerivationDescriptor,
        pending_create_new_chain_ids: BTreeSet<u64>,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.new_hardware_wallet_metadata_with_view(
            view,
            wallet_uuid,
            label,
            descriptor,
            pending_create_new_chain_ids,
        )
    }

    fn new_hardware_wallet_metadata_with_view(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
        label: &str,
        descriptor: HardwareDerivationDescriptor,
        pending_create_new_chain_ids: BTreeSet<u64>,
    ) -> Result<WalletMetadataBundle, VaultError> {
        descriptor
            .validate()
            .map_err(|_| VaultError::InvalidHardwareWalletDescriptor)?;
        let existing = self.list_wallet_metadata_with_view(view)?;
        Self::ensure_hardware_wallet_account_index_available(&existing, &descriptor)?;
        let label = validate_wallet_label(label, &existing, None)?;
        let display_order = next_wallet_display_order(&existing)?;
        let pending_create_new_chain_ids =
            if descriptor.sync_intent == HardwareWalletSyncIntent::CreateNew {
                pending_create_new_chain_ids
            } else {
                BTreeSet::new()
            };
        Ok(WalletMetadataBundle {
            wallet_uuid: wallet_uuid.to_owned(),
            label,
            derivation_index: descriptor.account_index,
            source: WalletSource::from_hardware_device_kind(descriptor.device_kind),
            status: WalletStatus::Active,
            display_order,
            hardware_descriptor: Some(descriptor),
            hardware_account: None,
            pending_create_new_chain_ids,
            software_context: None,
        })
    }

    pub fn complete_pending_create_new_chain_for_session(
        &self,
        view_session: &DesktopViewSession,
        chain_type: u8,
        chain_id: u64,
        contract: &str,
    ) -> Result<bool, VaultError> {
        let _guard = wallet_metadata_update_guard();
        let mut metadata = self.load_wallet_metadata_for_session(view_session)?;
        if !metadata.pending_create_new_chain_ids.contains(&chain_id)
            || self
                .find_wallet_chain_metadata_for_session(
                    view_session,
                    chain_type,
                    chain_id,
                    contract,
                )?
                .is_none()
        {
            return Ok(false);
        }

        metadata.pending_create_new_chain_ids.remove(&chain_id);
        self.store_wallet_metadata_with_view(&view_session.view, &metadata)?;
        Ok(true)
    }

    pub fn update_wallet_label(
        &self,
        password: &str,
        wallet_uuid: &str,
        label: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let view = self.unlock_view(password)?;
        self.update_wallet_label_with_view(&view, wallet_uuid, label)
    }

    pub fn update_wallet_label_for_session(
        &self,
        view_session: &DesktopViewSession,
        wallet_uuid: &str,
        label: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.update_wallet_label_with_view(&view_session.view, wallet_uuid, label)
    }

    pub fn update_wallet_label_with_view_unlock(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
        label: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.update_wallet_label_with_view(view, wallet_uuid, label)
    }

    fn update_wallet_label_with_view(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
        label: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let _guard = wallet_metadata_update_guard();
        let mut metadata = self.list_wallet_metadata_with_view_unlocked(view)?;
        let label = validate_wallet_label(label, &metadata, Some(wallet_uuid))?;
        let Some(target) = metadata
            .iter_mut()
            .find(|metadata| metadata.wallet_uuid == wallet_uuid)
        else {
            return Err(VaultError::WalletNotFound);
        };
        target.label = label;
        let updated = target.clone();
        self.store_wallet_metadata_with_view(view, &updated)?;
        Ok(updated)
    }

    pub fn set_standard_context_auto_open_preference(
        &self,
        password: &str,
        base_profile_uuid: &str,
        enabled: bool,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let view = self.unlock_view(password)?;
        self.set_standard_context_auto_open_preference_with_view_unlock(
            &view,
            base_profile_uuid,
            enabled,
        )
    }

    pub fn set_standard_context_auto_open_preference_with_view_unlock(
        &self,
        view: &ViewUnlock,
        base_profile_uuid: &str,
        enabled: bool,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let _guard = wallet_metadata_update_guard();
        let mut metadata = self.list_wallet_metadata_with_view_unlocked(view)?;
        let Some(target) = metadata.iter_mut().find(|metadata| {
            metadata.wallet_uuid == base_profile_uuid
                && metadata.software_context.as_ref().is_some_and(|context| {
                    context.kind == WalletSoftwareContextKind::Standard
                        && context.base_profile_uuid == base_profile_uuid
                })
        }) else {
            return Err(VaultError::WalletNotFound);
        };
        let Some(context) = target.software_context.as_mut() else {
            return Err(VaultError::InvalidWalletMetadata);
        };
        context.auto_open_standard_context = enabled;
        let updated = target.clone();
        self.store_wallet_metadata_with_view(view, &updated)?;
        Ok(updated)
    }

    pub fn reorder_active_wallets(
        &self,
        password: &str,
        ordered_wallet_uuids: &[String],
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let view = self.unlock_view(password)?;
        self.reorder_active_wallets_with_view(&view, ordered_wallet_uuids)
    }

    pub fn reorder_active_wallets_for_session(
        &self,
        view_session: &DesktopViewSession,
        ordered_wallet_uuids: &[String],
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        self.reorder_active_wallets_with_view(&view_session.view, ordered_wallet_uuids)
    }

    pub fn reorder_active_wallets_with_view_unlock(
        &self,
        view: &ViewUnlock,
        ordered_wallet_uuids: &[String],
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        self.reorder_active_wallets_with_view(view, ordered_wallet_uuids)
    }

    fn reorder_active_wallets_with_view(
        &self,
        view: &ViewUnlock,
        ordered_wallet_uuids: &[String],
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let _guard = wallet_metadata_update_guard();
        let mut metadata = self.list_wallet_metadata_with_view_unlocked(view)?;
        let active_ids = metadata
            .iter()
            .filter(|metadata| metadata.status == WalletStatus::Active)
            .map(|metadata| metadata.wallet_uuid.as_str())
            .collect::<BTreeSet<_>>();
        let submitted_ids = ordered_wallet_uuids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if active_ids != submitted_ids || submitted_ids.len() != ordered_wallet_uuids.len() {
            return Err(VaultError::InvalidWalletOrder);
        }

        for (display_order, wallet_uuid) in ordered_wallet_uuids.iter().enumerate() {
            let display_order =
                u32::try_from(display_order).map_err(|_| VaultError::WalletDisplayOrderOverflow)?;
            let Some(target) = metadata
                .iter_mut()
                .find(|metadata| metadata.wallet_uuid == *wallet_uuid)
            else {
                return Err(VaultError::InvalidWalletOrder);
            };
            target.display_order = display_order;
        }

        self.store_wallet_metadata_batch_with_view(view, &metadata)?;
        metadata.retain(|metadata| metadata.status == WalletStatus::Active);
        sort_wallet_metadata(&mut metadata);
        Ok(metadata)
    }

    pub fn deactivate_wallet(
        &self,
        password: &str,
        wallet_uuid: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let view = self.unlock_view(password)?;
        self.set_wallet_active_with_view(&view, wallet_uuid, false)
    }

    pub fn set_wallet_active_for_session(
        &self,
        view_session: &DesktopViewSession,
        wallet_uuid: &str,
        active: bool,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.set_wallet_active_with_view(&view_session.view, wallet_uuid, active)
    }

    pub fn set_wallet_active_with_view_unlock(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
        active: bool,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.set_wallet_active_with_view(view, wallet_uuid, active)
    }

    fn set_wallet_active_with_view(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
        active: bool,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let _guard = wallet_metadata_update_guard();
        let mut metadata = self.list_wallet_metadata_with_view_unlocked(view)?;
        let active_count = metadata
            .iter()
            .filter(|metadata| metadata.status == WalletStatus::Active)
            .count();
        let Some(target_index) = metadata
            .iter()
            .position(|metadata| metadata.wallet_uuid == wallet_uuid)
        else {
            return Err(VaultError::WalletNotFound);
        };

        let target_status = metadata[target_index].status;
        if active {
            if target_status == WalletStatus::Active {
                return Ok(metadata[target_index].clone());
            }
            metadata[target_index].status = WalletStatus::Active;
            metadata[target_index].display_order = next_wallet_display_order(&metadata)?;
        } else {
            if target_status == WalletStatus::Inactive {
                return Ok(metadata[target_index].clone());
            }
            if active_count <= 1 {
                return Err(VaultError::LastActiveWallet);
            }
            metadata[target_index].status = WalletStatus::Inactive;
        }

        let updated = metadata[target_index].clone();
        self.store_wallet_metadata_with_view(view, &updated)?;
        Ok(updated)
    }

    pub fn delete_wallet_for_session(
        &self,
        view_session: &DesktopViewSession,
        wallet_uuid: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.delete_wallet_with_view(&view_session.view, Some(view_session), wallet_uuid)
    }

    pub fn delete_wallet_with_view_unlock(
        &self,
        view: &ViewUnlock,
        wallet_uuid: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        self.delete_wallet_with_view(view, None, wallet_uuid)
    }

    pub fn delete_software_profile_with_view_unlock(
        &self,
        view: &ViewUnlock,
        base_profile_uuid: &str,
    ) -> Result<Vec<WalletMetadataBundle>, VaultError> {
        let _guard = wallet_metadata_update_guard();
        let metadata = self.list_wallet_metadata_with_view_unlocked(view)?;
        let base = standard_base_profile(&metadata, base_profile_uuid)?;
        let profile_ids = metadata
            .iter()
            .filter(|metadata| {
                metadata.wallet_uuid == base_profile_uuid
                    || metadata.software_context.as_ref().is_some_and(|context| {
                        context.kind == WalletSoftwareContextKind::Passphrase
                            && context.base_profile_uuid == base_profile_uuid
                    })
            })
            .map(|metadata| metadata.wallet_uuid.clone())
            .collect::<Vec<_>>();
        if !metadata.iter().any(|metadata| {
            metadata.status == WalletStatus::Active
                && !profile_ids
                    .iter()
                    .any(|wallet_id| wallet_id == &metadata.wallet_uuid)
        }) {
            return Err(VaultError::LastActiveWallet);
        }

        let mut deleted = Vec::with_capacity(profile_ids.len());
        for wallet_uuid in profile_ids
            .iter()
            .filter(|wallet_uuid| wallet_uuid.as_str() != base_profile_uuid)
        {
            deleted.push(self.delete_wallet_with_view_locked(view, None, wallet_uuid, false)?);
        }
        deleted.push(self.delete_wallet_with_view_locked(
            view,
            None,
            base.wallet_uuid.as_str(),
            true,
        )?);
        Ok(deleted)
    }

    fn delete_wallet_with_view(
        &self,
        view: &ViewUnlock,
        view_session: Option<&DesktopViewSession>,
        wallet_uuid: &str,
    ) -> Result<WalletMetadataBundle, VaultError> {
        let _guard = wallet_metadata_update_guard();
        self.delete_wallet_with_view_locked(view, view_session, wallet_uuid, false)
    }

    fn delete_wallet_with_view_locked(
        &self,
        view: &ViewUnlock,
        view_session: Option<&DesktopViewSession>,
        wallet_uuid: &str,
        allow_standard_children: bool,
    ) -> Result<WalletMetadataBundle, VaultError> {
        // Prevent an executor writer holding a view session from recreating records
        // between collecting the deletion keys and deleting the wallet.
        let _executor_guard = crate::vault::executors::EXECUTOR_RECORD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metadata = self.list_wallet_metadata_with_view_unlocked(view)?;
        let Some(target) = metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_uuid)
            .cloned()
        else {
            return Err(VaultError::WalletNotFound);
        };
        if !allow_standard_children
            && target
                .software_context
                .as_ref()
                .is_some_and(|context| context.kind == WalletSoftwareContextKind::Standard)
            && metadata.iter().any(|metadata| {
                metadata.software_context.as_ref().is_some_and(|context| {
                    context.kind == WalletSoftwareContextKind::Passphrase
                        && context.base_profile_uuid == target.wallet_uuid
                })
            })
        {
            return Err(VaultError::StandardWalletHasPassphraseChildren);
        }
        let active_count = metadata
            .iter()
            .filter(|metadata| metadata.status == WalletStatus::Active)
            .count();
        if target.status == WalletStatus::Active && active_count <= 1 {
            return Err(VaultError::LastActiveWallet);
        }
        if let Some(descriptor) = target.hardware_derivation_descriptor() {
            let session = view_session.ok_or(VaultError::HardwareWalletViewRequiresDevice)?;
            if session.wallet_id() != wallet_uuid {
                return Err(VaultError::HardwareWalletIdentityMismatch);
            }
            session
                .hardware_profile_session()
                .ok_or(VaultError::HardwareWalletIdentityMismatch)?
                .verify_descriptor(descriptor)?;
        }
        let mut keys_to_delete = vec![
            wallet_metadata_record_key(wallet_uuid),
            wallet_view_record_key(wallet_uuid),
            wallet_spend_record_key(wallet_uuid),
        ];
        let index_complete_key = wallet_chain_index_complete_record_key(wallet_uuid);
        let ownership_index_complete = self
            .db
            .get_desktop_wallet_vault_record(&index_complete_key)?
            .is_some_and(|payload| {
                rmp_serde::from_slice::<u8>(&payload)
                    .is_ok_and(|version| version == WALLET_CHAIN_INDEX_COMPLETE_VERSION)
            });
        keys_to_delete.push(index_complete_key);
        let mut wallet_private_namespaces = BTreeSet::new();
        let chain_index_prefix = wallet_chain_index_prefix(wallet_uuid);
        let mut wallet_chain_uuids = BTreeSet::new();
        let mut indexed_chain_metadata_keys = BTreeSet::new();

        if !ownership_index_complete {
            for stored in self
                .db
                .list_desktop_wallet_vault_records(WALLET_CHAIN_INDEX_PREFIX)?
            {
                let Some((_, wallet_chain_uuid)) = stored.key.rsplit_once('|') else {
                    continue;
                };
                if !wallet_chain_uuid.is_empty() {
                    indexed_chain_metadata_keys
                        .insert(wallet_chain_metadata_record_key(wallet_chain_uuid));
                }
            }
        }

        for stored in self
            .db
            .list_desktop_wallet_vault_records(&chain_index_prefix)?
        {
            let wallet_chain_uuid = stored
                .key
                .strip_prefix(&chain_index_prefix)
                .filter(|wallet_chain_uuid| !wallet_chain_uuid.is_empty())
                .ok_or(VaultError::WalletChainMetadataUnavailable)?
                .to_owned();
            let chain_id = rmp_serde::from_slice::<u64>(&stored.payload)
                .map_err(|_| VaultError::WalletChainMetadataUnavailable)?;
            wallet_chain_uuids.insert(wallet_chain_uuid.clone());
            wallet_private_namespaces
                .insert((chain_id, wallet_chain_uuid.parse::<WalletCacheKey>()?));
            keys_to_delete.push(stored.key);
            keys_to_delete.push(wallet_chain_metadata_record_key(&wallet_chain_uuid));
        }

        if !ownership_index_complete {
            let chain_records = self
                .db
                .list_desktop_wallet_vault_records(WALLET_CHAIN_METADATA_PREFIX)?;
            for stored in chain_records {
                let Some(wallet_chain_uuid) = stored
                    .key
                    .strip_prefix(WALLET_CHAIN_METADATA_PREFIX)
                    .map(str::to_owned)
                else {
                    continue;
                };
                let record = match rmp_serde::from_slice::<EncryptedRecord>(&stored.payload) {
                    Ok(record) => record,
                    Err(_) if indexed_chain_metadata_keys.contains(&stored.key) => continue,
                    Err(_) => return Err(VaultError::WalletChainMetadataUnavailable),
                };
                let mut authenticated_malformed = false;
                let metadata = view_session
                    .and_then(|session| {
                        match session.decrypt_wallet_chain_metadata(&wallet_chain_uuid, &record) {
                            Ok(metadata) => Some(metadata),
                            Err(VaultError::Decrypt) => None,
                            Err(_) => {
                                authenticated_malformed = true;
                                None
                            }
                        }
                    })
                    .or_else(|| {
                        match view.decrypt_wallet_chain_metadata(&wallet_chain_uuid, &record) {
                            Ok(metadata) => Some(metadata),
                            Err(VaultError::Decrypt) => None,
                            Err(_) => {
                                authenticated_malformed = true;
                                None
                            }
                        }
                    });
                let Some(metadata) = metadata else {
                    if authenticated_malformed && !indexed_chain_metadata_keys.contains(&stored.key)
                    {
                        return Err(VaultError::WalletChainMetadataUnavailable);
                    }
                    continue;
                };
                if metadata.wallet_uuid != wallet_uuid {
                    continue;
                }
                if wallet_chain_uuids.insert(wallet_chain_uuid.clone()) {
                    wallet_private_namespaces.insert((
                        metadata.chain_id,
                        wallet_chain_uuid.parse::<WalletCacheKey>()?,
                    ));
                }
                keys_to_delete.push(stored.key);
                keys_to_delete.push(wallet_chain_index_record_key(
                    wallet_uuid,
                    &wallet_chain_uuid,
                ));
            }
        }

        for record in self.db.list_desktop_wallet_vault_records(
            &crate::vault::executors::executor_wallet_prefix(wallet_uuid),
        )? {
            keys_to_delete.push(record.key);
        }

        if !wallet_private_namespaces.is_empty() {
            let alpha_wallet_cache_key = wallet_uuid.parse::<WalletCacheKey>()?;
            for chain_id in wallet_private_namespaces
                .iter()
                .map(|(chain_id, _)| *chain_id)
                .collect::<BTreeSet<_>>()
            {
                wallet_private_namespaces.insert((chain_id, alpha_wallet_cache_key.clone()));
            }
        }

        for account in self.list_public_account_metadata_with_view(view)? {
            if matches!(
                &account.scope,
                PublicAccountScope::PrivateWallet { wallet_uuid: scoped } if scoped == wallet_uuid
            ) {
                keys_to_delete.push(public_account_metadata_record_key(
                    &account.public_account_uuid,
                ));
                keys_to_delete.push(public_account_secret_record_key(
                    &account.public_account_uuid,
                ));
            }
        }

        let put_records = if let Some(descriptor) = target.hardware_derivation_descriptor() {
            let reservation = Self::hardware_wallet_account_index_reservation(descriptor);
            let record = hardware_wallet_account_index_record_entry(
                view,
                &generate_opaque_id()?,
                &reservation,
            )?;
            vec![record]
        } else {
            Vec::new()
        };
        let private_namespaces = wallet_private_namespaces
            .into_iter()
            .map(|(chain_id, wallet_id)| WalletPrivateNamespaceId::new(chain_id, wallet_id))
            .collect::<Vec<_>>();

        self.db.delete_wallet(&WalletDeletionBatch {
            private_namespaces: &private_namespaces,
            desktop_wallet_vault_delete_keys: &keys_to_delete,
            desktop_wallet_vault_put_records: &put_records,
        })?;
        Ok(target)
    }
}

fn standard_base_profile<'a>(
    metadata: &'a [WalletMetadataBundle],
    base_profile_uuid: &str,
) -> Result<&'a WalletMetadataBundle, VaultError> {
    let Some(base) = metadata
        .iter()
        .find(|metadata| metadata.wallet_uuid == base_profile_uuid)
    else {
        return Err(VaultError::WalletNotFound);
    };
    let Some(context) = base.software_context.as_ref() else {
        return Err(VaultError::InvalidWalletMetadata);
    };
    if base.source.is_hardware_derived()
        || context.kind != WalletSoftwareContextKind::Standard
        || context.base_profile_uuid != base.wallet_uuid
    {
        return Err(VaultError::InvalidWalletMetadata);
    }
    Ok(base)
}
