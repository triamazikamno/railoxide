use super::{
    ConfirmedHardwarePublicAccount, DesktopVaultStore, DesktopViewSession, EncryptedRecord,
    KEY_LEN, PUBLIC_ACCOUNT_METADATA_PREFIX, ProtectedSoftwareSeedSession, PublicAccountMetadata,
    PublicAccountScope, PublicAccountSecret, PublicAccountSource, PublicAccountStatus,
    SoftwareSeedSessionBinding, SpendGrant, VaultError, ViewUnlock, WalletSoftwareContextKind,
    Zeroizing, derive_public_evm_address_from_entropy, derive_public_evm_address_from_seed,
    derive_public_evm_private_key_from_entropy, derive_public_evm_private_key_from_seed,
    ensure_public_account_address_available, generate_opaque_id, next_derived_public_account_index,
    next_public_account_display_order, normalize_public_account_label,
    parse_public_evm_private_key, public_account_metadata_record_entry,
    public_account_metadata_record_key, public_account_secret_record_entry,
    public_account_secret_record_key, public_evm_address_from_private_key,
    sort_public_account_metadata, unlock_spend, unlock_view, wallet_spend_record_key,
};

impl DesktopVaultStore {
    pub fn list_active_public_accounts_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<PublicAccountMetadata>, VaultError> {
        self.list_public_accounts_for_session(view_session, false)
    }

    pub fn list_public_accounts_for_session(
        &self,
        view_session: &DesktopViewSession,
        include_inactive: bool,
    ) -> Result<Vec<PublicAccountMetadata>, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let wallet_id = view_session.wallet_id();
        accounts.retain(|account| {
            account.is_scoped_to_wallet(wallet_id)
                && (include_inactive || account.status == PublicAccountStatus::Active)
        });
        sort_public_account_metadata(&mut accounts);
        Ok(accounts)
    }

    /// Every public account in the vault, regardless of wallet scope; for
    /// cross-wallet display lookups only.
    pub fn list_all_public_accounts(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<PublicAccountMetadata>, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        sort_public_account_metadata(&mut accounts);
        Ok(accounts)
    }

    pub fn next_derived_public_account_index_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<u32, VaultError> {
        let accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        next_derived_public_account_index(&accounts, view_session.wallet_id())
    }

    pub fn add_derived_public_account(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        label: Option<&str>,
    ) -> Result<PublicAccountMetadata, VaultError> {
        self.add_derived_public_account_with_session(password, view_session, label, None)
    }

    pub fn add_derived_public_account_with_session(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        label: Option<&str>,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let mut grant = SpendGrant::one_use(unlock_spend(&vault_metadata, password)?);
        let wallet_id = view_session.wallet_id();
        let accounts = self.list_public_account_metadata_with_view(&view)?;
        let derivation_index = next_derived_public_account_index(&accounts, wallet_id)?;
        let metadata = self.load_wallet_metadata_with_view(&view, wallet_id)?;
        let Some(context) = metadata.software_context.as_ref() else {
            return Err(VaultError::InvalidWalletMetadata);
        };
        let address = match context.kind {
            WalletSoftwareContextKind::Passphrase => {
                let session =
                    protected_seed_session.ok_or(VaultError::SoftwareSeedSessionRequired)?;
                let binding = SoftwareSeedSessionBinding::new(
                    &context.base_profile_uuid,
                    wallet_id,
                    session.binding().vault_session_id(),
                );
                let seed = session.open(&mut grant, &binding)?;
                derive_public_evm_address_from_seed(&seed, derivation_index)?
            }
            WalletSoftwareContextKind::Standard => {
                let spend = grant.take_spend_unlock()?;
                let spend_record = self.encrypted_record(&wallet_spend_record_key(wallet_id))?;
                let spend_bundle = spend.decrypt_spend_bundle(wallet_id, &spend_record)?;
                derive_public_evm_address_from_entropy(
                    &spend_bundle.bip39_entropy,
                    derivation_index,
                )?
            }
        };
        ensure_public_account_address_available(
            &accounts,
            address,
            &PublicAccountScope::PrivateWallet {
                wallet_uuid: wallet_id.to_owned(),
            },
            wallet_id,
        )?;

        let account = PublicAccountMetadata {
            public_account_uuid: generate_opaque_id()?,
            address,
            label: normalize_public_account_label(label),
            source: PublicAccountSource::Derived,
            scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: wallet_id.to_owned(),
            },
            derivation_index: Some(derivation_index),
            hardware_descriptor: None,
            status: PublicAccountStatus::Active,
            display_order: next_public_account_display_order(&accounts)?,
        };
        let (key, data) = public_account_metadata_record_entry(&view, &account)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(account)
    }

    pub fn add_hardware_public_account(
        &self,
        view_session: &DesktopViewSession,
        confirmed_account: &ConfirmedHardwarePublicAccount,
        label: Option<&str>,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let descriptor = confirmed_account.descriptor().clone();
        let address = confirmed_account.address();
        descriptor
            .validate()
            .map_err(|_| VaultError::InvalidHardwareWalletDescriptor)?;
        let accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let wallet_id = view_session.wallet_id();
        let hardware_session = view_session
            .hardware_profile_session()
            .ok_or(VaultError::HardwareWalletViewRequiresDevice)?;
        let wallet_metadata = self.load_wallet_metadata_with_view(&view_session.view, wallet_id)?;
        let hardware_account = wallet_metadata
            .hardware_account
            .as_ref()
            .ok_or(VaultError::HardwareWalletViewRequiresDevice)?;
        Self::ensure_supported_hardware_account(hardware_account)?;
        hardware_session.verify_account(hardware_account)?;
        if descriptor.device_kind != hardware_account.descriptor.device_kind {
            return Err(VaultError::InvalidHardwareWalletDescriptor);
        }
        let derivation_index = next_derived_public_account_index(&accounts, wallet_id)?;
        if descriptor.wallet_account_index != view_session.derivation_index()
            || descriptor.public_account_index != derivation_index
        {
            return Err(VaultError::InvalidHardwareWalletDescriptor);
        }
        let scope = PublicAccountScope::PrivateWallet {
            wallet_uuid: wallet_id.to_owned(),
        };
        ensure_public_account_address_available(&accounts, address, &scope, wallet_id)?;

        let account = PublicAccountMetadata {
            public_account_uuid: generate_opaque_id()?,
            address,
            label: normalize_public_account_label(label),
            source: PublicAccountSource::HardwareDerived,
            scope,
            derivation_index: Some(derivation_index),
            hardware_descriptor: Some(descriptor),
            status: PublicAccountStatus::Active,
            display_order: next_public_account_display_order(&accounts)?,
        };
        let (key, data) = public_account_metadata_record_entry(&view_session.view, &account)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(account)
    }

    pub fn import_public_account(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        private_key_hex: &str,
        label: Option<&str>,
        global: bool,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let spend = unlock_spend(&vault_metadata, password)?;
        let private_key = parse_public_evm_private_key(private_key_hex)?;
        let address = public_evm_address_from_private_key(&private_key)?;
        let accounts = self.list_public_account_metadata_with_view(&view)?;
        let scope = if global {
            PublicAccountScope::Global
        } else {
            PublicAccountScope::PrivateWallet {
                wallet_uuid: view_session.wallet_id().to_owned(),
            }
        };
        ensure_public_account_address_available(
            &accounts,
            address,
            &scope,
            view_session.wallet_id(),
        )?;

        let account = PublicAccountMetadata {
            public_account_uuid: generate_opaque_id()?,
            address,
            label: normalize_public_account_label(label),
            source: PublicAccountSource::Imported,
            scope,
            derivation_index: None,
            hardware_descriptor: None,
            status: PublicAccountStatus::Active,
            display_order: next_public_account_display_order(&accounts)?,
        };
        let secret = PublicAccountSecret {
            private_key: *private_key,
        };
        let metadata_entry = public_account_metadata_record_entry(&view, &account)?;
        let secret_entry = public_account_secret_record_entry(&spend, &account, &secret)?;
        self.db
            .put_desktop_wallet_vault_records(&[metadata_entry, secret_entry])?;
        Ok(account)
    }

    pub fn update_public_account_label(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
        label: Option<&str>,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let Some(account) = accounts
            .iter_mut()
            .find(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        if !account.is_scoped_to_wallet(view_session.wallet_id()) {
            return Err(VaultError::PublicAccountNotFound);
        }
        account.label = normalize_public_account_label(label);
        let updated = account.clone();
        let (key, data) = public_account_metadata_record_entry(&view_session.view, &updated)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(updated)
    }

    pub fn deactivate_derived_public_account(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let Some(account) = accounts
            .iter_mut()
            .find(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        if !account.is_active_for_wallet(view_session.wallet_id()) {
            return Err(VaultError::PublicAccountNotFound);
        }
        if !matches!(
            account.source,
            PublicAccountSource::Derived
                | PublicAccountSource::HardwareDerived
                | PublicAccountSource::ExecutorDerived(_)
        ) {
            return Err(VaultError::InvalidPublicAccountOperation);
        }
        account.status = PublicAccountStatus::Inactive;
        let updated = account.clone();
        let (key, data) = public_account_metadata_record_entry(&view_session.view, &updated)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(updated)
    }

    pub fn activate_derived_public_account(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let mut accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let Some(account_index) = accounts
            .iter()
            .position(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        let account = &accounts[account_index];
        if !account.is_scoped_to_wallet(view_session.wallet_id()) {
            return Err(VaultError::PublicAccountNotFound);
        }
        if !matches!(
            account.source,
            PublicAccountSource::Derived
                | PublicAccountSource::HardwareDerived
                | PublicAccountSource::ExecutorDerived(_)
        ) {
            return Err(VaultError::InvalidPublicAccountOperation);
        }
        if account.status == PublicAccountStatus::Inactive {
            ensure_public_account_address_available(
                &accounts,
                account.address,
                &account.scope,
                view_session.wallet_id(),
            )?;
            accounts[account_index].status = PublicAccountStatus::Active;
        }
        let updated = accounts[account_index].clone();
        let (key, data) = public_account_metadata_record_entry(&view_session.view, &updated)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(updated)
    }

    pub fn delete_imported_public_account(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
    ) -> Result<PublicAccountMetadata, VaultError> {
        let accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
        let Some(account) = accounts
            .into_iter()
            .find(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        if !account.is_active_for_wallet(view_session.wallet_id()) {
            return Err(VaultError::PublicAccountNotFound);
        }
        if account.source != PublicAccountSource::Imported {
            return Err(VaultError::InvalidPublicAccountOperation);
        }

        self.db
            .delete_desktop_wallet_vault_record(&public_account_metadata_record_key(
                &account.public_account_uuid,
            ))?;
        self.db
            .delete_desktop_wallet_vault_record(&public_account_secret_record_key(
                &account.public_account_uuid,
            ))?;
        Ok(account)
    }

    pub fn public_account_signing_key(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
        self.public_account_signing_key_with_session(grant, view_session, public_account_uuid, None)
    }

    pub fn public_account_signing_key_with_session(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
        let accounts = self.list_public_accounts_for_session(view_session, true)?;
        let Some(account) = accounts
            .into_iter()
            .find(|account| account.public_account_uuid == public_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        match account.source {
            PublicAccountSource::Derived => {
                let Some(derivation_index) = account.derivation_index else {
                    return Err(VaultError::InvalidPublicAccountOperation);
                };
                let wallet_id = view_session.wallet_id();
                let metadata =
                    self.load_wallet_metadata_with_view(&view_session.view, wallet_id)?;
                let Some(context) = metadata.software_context.as_ref() else {
                    return Err(VaultError::InvalidWalletMetadata);
                };
                match context.kind {
                    WalletSoftwareContextKind::Passphrase => {
                        let session = protected_seed_session
                            .ok_or(VaultError::SoftwareSeedSessionRequired)?;
                        let binding = SoftwareSeedSessionBinding::new(
                            &context.base_profile_uuid,
                            wallet_id,
                            session.binding().vault_session_id(),
                        );
                        let seed = session.open(grant, &binding)?;
                        let private_key =
                            derive_public_evm_private_key_from_seed(&seed, derivation_index)?;
                        let address = public_evm_address_from_private_key(&private_key)?;
                        if address != account.address {
                            return Err(VaultError::InvalidSoftwareContextIdentity);
                        }
                        Ok(private_key)
                    }
                    WalletSoftwareContextKind::Standard => {
                        let spend = grant.take_spend_unlock()?;
                        let spend_record =
                            self.encrypted_record(&wallet_spend_record_key(wallet_id))?;
                        let spend_bundle = spend.decrypt_spend_bundle(wallet_id, &spend_record)?;
                        derive_public_evm_private_key_from_entropy(
                            &spend_bundle.bip39_entropy,
                            derivation_index,
                        )
                    }
                }
            }
            PublicAccountSource::Imported => {
                let spend = grant.take_spend_unlock()?;
                let record = self.encrypted_record(&public_account_secret_record_key(
                    &account.public_account_uuid,
                ))?;
                let secret =
                    spend.decrypt_public_account_secret(&account.public_account_uuid, &record)?;
                Ok(Zeroizing::new(secret.private_key))
            }
            // Executor keys may only be obtained by the live native owner while
            // its signing admission remains held through handoff.
            PublicAccountSource::HardwareDerived | PublicAccountSource::ExecutorDerived(_) => {
                Err(VaultError::InvalidPublicAccountOperation)
            }
        }
    }

    pub(in crate::vault) fn list_public_account_metadata_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<PublicAccountMetadata>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(PUBLIC_ACCOUNT_METADATA_PREFIX)?;
        let mut accounts = Vec::with_capacity(records.len());
        for stored in records {
            let Some(public_account_uuid) = stored.key.strip_prefix(PUBLIC_ACCOUNT_METADATA_PREFIX)
            else {
                continue;
            };
            let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
            let mut account = view.decrypt_public_account_metadata(public_account_uuid, &record)?;
            if account.public_account_uuid != public_account_uuid {
                public_account_uuid.clone_into(&mut account.public_account_uuid);
            }
            accounts.push(account);
        }
        sort_public_account_metadata(&mut accounts);
        Ok(accounts)
    }
}
