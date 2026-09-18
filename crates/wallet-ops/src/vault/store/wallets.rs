use super::{
    DesktopVaultStore, DesktopViewSession, GeneratedSeedMaterial, ProtectedSoftwareSeedSession,
    PublicAccountScope, PublicAccountSource, SoftwareRailgunSpendSigner,
    SoftwareSeedSessionBinding, SpendGrant, StoredWalletRecord, VaultError, VaultRecordEntries,
    ViewUnlock, WALLET_VIEW_PREFIX, WalletKeys, WalletMetadataBundle, WalletSoftwareContextKind,
    WalletSpendBundle, WalletViewBundle, Zeroizing, bip39_entropy_from_mnemonic,
    bip39_mnemonic_from_entropy, derive_public_evm_address_from_seed,
    initial_derived_public_account, public_account_metadata_record_entry, unlock_spend,
    unlock_view, wallet_chain_index_complete_record_entry, wallet_keys_from_mnemonic,
    wallet_keys_from_seed, wallet_metadata_record_key, wallet_spend_record_key,
    wallet_view_record_key, zeroize_wallet_keys,
};

impl DesktopVaultStore {
    pub fn store_wallet_from_entropy(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        bip39_language: impl Into<String>,
        entropy: &[u8],
    ) -> Result<StoredWalletRecord, VaultError> {
        let (stored, records) = self.encrypted_wallet_records_from_entropy(
            password,
            wallet_id,
            derivation_index,
            bip39_language.into(),
            entropy,
            None,
        )?;
        self.db.put_desktop_wallet_vault_records(&records)?;
        Ok(stored)
    }

    pub fn store_wallet_from_entropy_with_metadata(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        bip39_language: impl Into<String>,
        entropy: &[u8],
        metadata: &WalletMetadataBundle,
    ) -> Result<StoredWalletRecord, VaultError> {
        let (stored, records) = self.encrypted_wallet_records_from_entropy(
            password,
            wallet_id,
            derivation_index,
            bip39_language.into(),
            entropy,
            Some(metadata),
        )?;
        self.db.put_desktop_wallet_vault_records(&records)?;
        Ok(stored)
    }

    pub fn store_generated_wallet(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        bip39_language: impl Into<String>,
        seed: &GeneratedSeedMaterial,
    ) -> Result<StoredWalletRecord, VaultError> {
        self.store_wallet_from_entropy(
            password,
            wallet_id,
            derivation_index,
            bip39_language,
            &seed.entropy,
        )
    }

    pub fn store_generated_wallet_with_metadata(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        bip39_language: impl Into<String>,
        seed: &GeneratedSeedMaterial,
        metadata: &WalletMetadataBundle,
    ) -> Result<StoredWalletRecord, VaultError> {
        self.store_wallet_from_entropy_with_metadata(
            password,
            wallet_id,
            derivation_index,
            bip39_language,
            &seed.entropy,
            metadata,
        )
    }

    pub fn import_wallet_mnemonic(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        bip39_language: impl Into<String>,
        mnemonic: &str,
    ) -> Result<StoredWalletRecord, VaultError> {
        let entropy = Zeroizing::new(bip39_entropy_from_mnemonic(mnemonic)?);
        self.store_wallet_from_entropy(
            password,
            wallet_id,
            derivation_index,
            bip39_language,
            &entropy,
        )
    }

    pub fn import_wallet_mnemonic_with_metadata(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        bip39_language: impl Into<String>,
        mnemonic: &str,
        metadata: &WalletMetadataBundle,
    ) -> Result<StoredWalletRecord, VaultError> {
        let entropy = Zeroizing::new(bip39_entropy_from_mnemonic(mnemonic)?);
        self.store_wallet_from_entropy_with_metadata(
            password,
            wallet_id,
            derivation_index,
            bip39_language,
            &entropy,
            metadata,
        )
    }

    pub fn load_view_bundle(
        &self,
        password: &str,
        wallet_id: &str,
    ) -> Result<WalletViewBundle, VaultError> {
        let view = self.unlock_view(password)?;
        self.ensure_password_view_allowed(&view, wallet_id)?;
        let record = self.encrypted_record(&wallet_view_record_key(wallet_id))?;
        view.decrypt_view_bundle(wallet_id, &record)
    }

    pub fn list_wallet_ids(&self) -> Result<Vec<String>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(WALLET_VIEW_PREFIX)?;
        Ok(records
            .into_iter()
            .filter_map(|record| {
                record
                    .key
                    .strip_prefix(WALLET_VIEW_PREFIX)
                    .map(str::to_owned)
            })
            .collect())
    }

    pub fn load_view_session(
        &self,
        password: &str,
        wallet_id: &str,
    ) -> Result<DesktopViewSession, VaultError> {
        let view = self.unlock_view(password)?;
        self.ensure_password_view_allowed(&view, wallet_id)?;
        let record = self.encrypted_record(&wallet_view_record_key(wallet_id))?;
        let bundle = view.decrypt_view_bundle(wallet_id, &record)?;
        Ok(DesktopViewSession::from_bundle(
            wallet_id.to_owned(),
            &bundle,
            view,
        ))
    }

    pub fn load_view_session_with_view_session(
        &self,
        view_session: &DesktopViewSession,
        wallet_id: &str,
    ) -> Result<DesktopViewSession, VaultError> {
        self.ensure_password_view_allowed(&view_session.view, wallet_id)?;
        let record = self.encrypted_record(&wallet_view_record_key(wallet_id))?;
        let bundle = view_session.view.decrypt_view_bundle(wallet_id, &record)?;
        Ok(DesktopViewSession::from_bundle(
            wallet_id.to_owned(),
            &bundle,
            view_session.view.clone_unlock(),
        ))
    }

    pub fn load_view_session_with_view_unlock(
        &self,
        view: &ViewUnlock,
        wallet_id: &str,
    ) -> Result<DesktopViewSession, VaultError> {
        self.ensure_password_view_allowed(view, wallet_id)?;
        let record = self.encrypted_record(&wallet_view_record_key(wallet_id))?;
        let bundle = view.decrypt_view_bundle(wallet_id, &record)?;
        Ok(DesktopViewSession::from_bundle(
            wallet_id.to_owned(),
            &bundle,
            view.clone_unlock(),
        ))
    }

    pub fn unlock_first_view_session(
        &self,
        password: &str,
    ) -> Result<Option<DesktopViewSession>, VaultError> {
        let view = self.unlock_view(password)?;
        let wallet_ids = self.list_wallet_ids()?;
        if wallet_ids.is_empty() {
            return Ok(None);
        }
        for wallet_id in wallet_ids {
            match self.ensure_password_view_allowed(&view, &wallet_id) {
                Ok(()) => {
                    let record = self.encrypted_record(&wallet_view_record_key(&wallet_id))?;
                    let bundle = view.decrypt_view_bundle(&wallet_id, &record)?;
                    return Ok(Some(DesktopViewSession::from_bundle(
                        wallet_id, &bundle, view,
                    )));
                }
                Err(
                    VaultError::HardwareWalletViewRequiresDevice
                    | VaultError::UnsupportedHardwareCustodyBackend(_),
                ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    pub fn load_spend_bundle(
        &self,
        grant: &mut SpendGrant,
        wallet_id: &str,
    ) -> Result<WalletSpendBundle, VaultError> {
        let record = self.encrypted_record(&wallet_spend_record_key(wallet_id))?;
        grant
            .take_spend_unlock()?
            .decrypt_spend_bundle(wallet_id, &record)
    }

    pub fn railgun_spend_signer(
        &self,
        grant: &mut SpendGrant,
        wallet_id: &str,
    ) -> Result<SoftwareRailgunSpendSigner, VaultError> {
        let bundle = self.load_spend_bundle(grant, wallet_id)?;
        let mnemonic = Zeroizing::new(bip39_mnemonic_from_entropy(&bundle.bip39_entropy)?);
        let wallet = wallet_keys_from_mnemonic(&mnemonic, "", bundle.derivation_index)?;
        Ok(SoftwareRailgunSpendSigner { wallet })
    }

    pub fn railgun_spend_signer_for_session(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<SoftwareRailgunSpendSigner, VaultError> {
        self.software_seed_and_signer_for_session(grant, view_session, protected_seed_session)
            .map(|(_seed, signer)| signer)
    }

    /// Derives both signers from the same authorized, identity-checked software seed.
    /// Native owners must additionally validate their operation and session generation.
    pub fn executor_spend_signers_for_session(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
        chain_id: u64,
        executor_index: u32,
    ) -> Result<
        (
            SoftwareRailgunSpendSigner,
            alloy::signers::local::PrivateKeySigner,
        ),
        crate::vault::ExecutorStoreError,
    > {
        if view_session.hardware_profile_session().is_some() {
            return Err(crate::vault::ExecutorStoreError::SoftwareWalletRequired);
        }
        let (seed, private_signer) =
            self.software_seed_and_signer_for_session(grant, view_session, protected_seed_session)?;
        let executor_signer = railgun_wallet::keys::derive_executor_signer(
            &seed,
            view_session.derivation_index(),
            chain_id,
            executor_index,
        )
        .map_err(VaultError::from)?;
        Ok((private_signer, executor_signer))
    }

    /// Derive the reserved operation and optional spare during one authorized seed access.
    /// No signing keys escape preparation or enter its background inspection.
    pub(crate) fn executor_preparation_addresses_for_session(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
        chain_id: u64,
        executor_index: u32,
        spare_index: Option<u32>,
    ) -> Result<
        (
            alloy::primitives::Address,
            Option<alloy::primitives::Address>,
        ),
        crate::vault::ExecutorStoreError,
    > {
        if view_session.hardware_profile_session().is_some() {
            return Err(crate::vault::ExecutorStoreError::SoftwareWalletRequired);
        }
        let (seed, _private_signer) =
            self.software_seed_and_signer_for_session(grant, view_session, protected_seed_session)?;
        let address = |index| {
            railgun_wallet::keys::derive_executor_signer(
                &seed,
                view_session.derivation_index(),
                chain_id,
                index,
            )
            .map(|signer| signer.address())
            .map_err(VaultError::from)
        };
        Ok((
            address(executor_index)?,
            spare_index.map(address).transpose()?,
        ))
    }

    /// Derive only the addresses in an explicitly requested recovery range.
    /// No signing keys escape this read/discovery operation.
    pub fn executor_addresses_for_session(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
        chain_id: u64,
        range: std::ops::Range<u32>,
    ) -> Result<Vec<(u32, alloy::primitives::Address)>, crate::vault::ExecutorStoreError> {
        if view_session.hardware_profile_session().is_some() {
            return Err(crate::vault::ExecutorStoreError::SoftwareWalletRequired);
        }
        if range.is_empty()
            || range.end > (1 << 31)
            || range.end - range.start > crate::vault::MAX_EXECUTOR_DISCOVERY_RANGE
        {
            return Err(crate::vault::ExecutorStoreError::DiscoveryRange);
        }
        let (seed, _private_signer) =
            self.software_seed_and_signer_for_session(grant, view_session, protected_seed_session)?;
        range
            .map(|index| {
                railgun_wallet::keys::derive_executor_signer(
                    &seed,
                    view_session.derivation_index(),
                    chain_id,
                    index,
                )
                .map(|signer| (index, signer.address()))
                .map_err(|error| crate::vault::ExecutorStoreError::Vault(VaultError::from(error)))
            })
            .collect()
    }

    fn software_seed_and_signer_for_session(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<(Zeroizing<[u8; 64]>, SoftwareRailgunSpendSigner), VaultError> {
        let wallet_id = view_session.wallet_id();
        let metadata = self.load_wallet_metadata_with_view(&view_session.view, wallet_id)?;
        let Some(context) = metadata.software_context.as_ref() else {
            return Err(VaultError::InvalidWalletMetadata);
        };
        let (seed, wallet) = match context.kind {
            WalletSoftwareContextKind::Standard => {
                let bundle = self.load_spend_bundle(grant, wallet_id)?;
                let mnemonic = Zeroizing::new(bip39_mnemonic_from_entropy(&bundle.bip39_entropy)?);
                let seed = crate::vault::bip39_seed_from_mnemonic(&mnemonic, "")?;
                let wallet = wallet_keys_from_seed(&seed, bundle.derivation_index)?;
                (seed, wallet)
            }
            WalletSoftwareContextKind::Passphrase => {
                let session =
                    protected_seed_session.ok_or(VaultError::SoftwareSeedSessionRequired)?;
                let binding = SoftwareSeedSessionBinding::new(
                    &context.base_profile_uuid,
                    wallet_id,
                    session.binding().vault_session_id(),
                );
                let seed = session.open(grant, &binding)?;
                let wallet = wallet_keys_from_seed(&seed, view_session.derivation_index())?;
                let accounts = self.list_public_account_metadata_with_view(&view_session.view)?;
                let identity_accounts = accounts
                    .iter()
                    .filter(|account| {
                        account.source == PublicAccountSource::Derived
                            && account.derivation_index == Some(0)
                            && matches!(
                                &account.scope,
                                PublicAccountScope::PrivateWallet { wallet_uuid }
                                    if wallet_uuid == view_session.wallet_id()
                            )
                    })
                    .collect::<Vec<_>>();
                if identity_accounts.len() != 1
                    || identity_accounts[0].address
                        != derive_public_evm_address_from_seed(&seed, 0)?
                {
                    let mut wallet = wallet;
                    zeroize_wallet_keys(&mut wallet);
                    return Err(VaultError::InvalidSoftwareContextIdentity);
                }
                (seed, wallet)
            }
        };
        let view_record = self.encrypted_record(&wallet_view_record_key(wallet_id))?;
        let persisted_view = view_session
            .view
            .decrypt_view_bundle(wallet_id, &view_record)?;
        if persisted_view.derivation_index != view_session.derivation_index()
            || !wallet_identity_matches_view_bundle(&wallet, &persisted_view)
        {
            let mut wallet = wallet;
            zeroize_wallet_keys(&mut wallet);
            return Err(VaultError::InvalidSoftwareContextIdentity);
        }
        Ok((seed, SoftwareRailgunSpendSigner { wallet }))
    }

    fn encrypted_wallet_records_from_entropy(
        &self,
        password: &str,
        wallet_id: &str,
        derivation_index: u32,
        bip39_language: String,
        entropy: &[u8],
        metadata: Option<&WalletMetadataBundle>,
    ) -> Result<(StoredWalletRecord, VaultRecordEntries), VaultError> {
        if metadata
            .and_then(|metadata| metadata.software_context.as_ref())
            .is_some_and(|context| context.kind == WalletSoftwareContextKind::Passphrase)
        {
            return Err(VaultError::InvalidWalletMetadata);
        }
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let spend = unlock_spend(&vault_metadata, password)?;
        let mnemonic = Zeroizing::new(bip39_mnemonic_from_entropy(entropy)?);
        let mut wallet = wallet_keys_from_mnemonic(&mnemonic, "", derivation_index)?;
        let view_bundle = WalletViewBundle::from_wallet_keys(derivation_index, &wallet);
        zeroize_wallet_keys(&mut wallet);
        let spend_bundle = WalletSpendBundle {
            derivation_index,
            bip39_language,
            bip39_entropy: entropy.to_vec(),
        };
        let existing_public_accounts = if metadata.is_some() {
            self.list_public_account_metadata_with_view(&view)?
        } else {
            Vec::new()
        };

        let view_record = view.encrypt_view_bundle(wallet_id, &view_bundle)?;
        let spend_record = spend.encrypt_spend_bundle(wallet_id, &spend_bundle)?;
        let view_record_key = wallet_view_record_key(wallet_id);
        let spend_record_key = wallet_spend_record_key(wallet_id);
        let mut records = Vec::with_capacity(2 + usize::from(metadata.is_some()) * 3);
        records.push(view_record.to_record_entry(view_record_key.clone())?);
        records.push(spend_record.to_record_entry(spend_record_key.clone())?);

        if let Some(metadata) = metadata {
            let record = view.encrypt_wallet_metadata(&metadata.wallet_uuid, metadata)?;
            records
                .push(record.to_record_entry(wallet_metadata_record_key(&metadata.wallet_uuid))?);
            records.push(wallet_chain_index_complete_record_entry(
                &metadata.wallet_uuid,
            )?);

            let public_account =
                initial_derived_public_account(wallet_id, entropy, &existing_public_accounts)?;
            records.push(public_account_metadata_record_entry(
                &view,
                &public_account,
            )?);
        }

        Ok((
            StoredWalletRecord {
                wallet_id: wallet_id.to_string(),
                derivation_index,
                view_record_key,
                spend_record_key,
            },
            records,
        ))
    }
}

fn wallet_identity_matches_view_bundle(wallet: &WalletKeys, bundle: &WalletViewBundle) -> bool {
    wallet.spending_public_key.map(|value| value.to_be_bytes()) == bundle.spending_public_key
        && wallet.viewing.viewing_public_key == bundle.viewing_public_key
        && wallet.viewing.nullifying_key.to_be_bytes() == bundle.nullifying_key
        && wallet.viewing.master_public_key.to_be_bytes() == bundle.master_public_key
}
