use super::{
    CreateSoftwareContextResult, DesktopVaultStore, ProtectedSoftwareSeedSession,
    PublicAccountMetadata, PublicAccountScope, PublicAccountSource, SoftwareContextChainInput,
    SoftwareContextSyncIntent, SoftwareSeedSessionBinding, SpendGrant, SpendUnlock, VaultError,
    VaultSessionId, ViewUnlock, WalletChainMetadataBundle, WalletKeys, WalletMetadataBundle,
    WalletSoftwareContext, WalletSoftwareContextKind, WalletStatus, bip39_mnemonic_from_entropy,
    bip39_seed_from_mnemonic_zeroizing, derive_public_evm_address_from_seed, generate_opaque_id,
    initial_derived_public_account_from_seed, next_wallet_display_order,
    public_account_metadata_record_entry, validate_wallet_label,
    wallet_chain_index_complete_record_entry, wallet_chain_index_record_key,
    wallet_chain_metadata_record_key, wallet_keys_from_seed, wallet_metadata_record_key,
    wallet_spend_record_key, wallet_view_record_key, zeroize_wallet_keys,
};
use zeroize::Zeroizing;

pub enum SoftwareContextMatch {
    Known {
        metadata: Box<WalletMetadataBundle>,
        session: ProtectedSoftwareSeedSession,
    },
    Unknown,
}

impl SoftwareContextMatch {
    #[must_use]
    pub const fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }
}

struct CandidateContext {
    metadata: WalletMetadataBundle,
    view_bundle: super::WalletViewBundle,
    public_account: Option<PublicAccountMetadata>,
}

impl DesktopVaultStore {
    pub fn create_software_context(
        &self,
        view: &ViewUnlock,
        grant: &mut SpendGrant,
        base_profile_uuid: &str,
        context_wallet_uuid: &str,
        railgun_derivation_index: u32,
        label: &str,
        passphrase: Zeroizing<String>,
        passphrase_confirmation: Zeroizing<String>,
        intent: SoftwareContextSyncIntent,
        chains: &[SoftwareContextChainInput],
        vault_session_id: VaultSessionId,
    ) -> Result<CreateSoftwareContextResult, VaultError> {
        if passphrase.as_bytes() != passphrase_confirmation.as_bytes() {
            return Err(VaultError::PassphraseConfirmationMismatch);
        }
        if passphrase.is_empty() {
            return Err(VaultError::EmptySoftwareContextPassphrase);
        }
        let spend = grant.take_spend_unlock()?;
        self.create_software_context_with_spend_unlock(
            view,
            &spend,
            base_profile_uuid,
            context_wallet_uuid,
            railgun_derivation_index,
            label,
            passphrase,
            passphrase_confirmation,
            intent,
            chains,
            vault_session_id,
        )
    }

    pub fn create_software_context_with_spend_unlock(
        &self,
        view: &ViewUnlock,
        spend: &SpendUnlock,
        base_profile_uuid: &str,
        context_wallet_uuid: &str,
        railgun_derivation_index: u32,
        label: &str,
        passphrase: Zeroizing<String>,
        passphrase_confirmation: Zeroizing<String>,
        intent: SoftwareContextSyncIntent,
        chains: &[SoftwareContextChainInput],
        vault_session_id: VaultSessionId,
    ) -> Result<CreateSoftwareContextResult, VaultError> {
        if passphrase.as_bytes() != passphrase_confirmation.as_bytes() {
            return Err(VaultError::PassphraseConfirmationMismatch);
        }
        if passphrase.is_empty() {
            return Err(VaultError::EmptySoftwareContextPassphrase);
        }
        drop(passphrase_confirmation);

        let existing = self.list_wallet_metadata_with_view(view)?;
        let base_profile = existing
            .iter()
            .find(|metadata| metadata.wallet_uuid == base_profile_uuid)
            .ok_or(VaultError::WalletNotFound)?;
        validate_standard_base_profile(base_profile)?;
        base_profile.validate()?;
        if context_wallet_uuid.is_empty() {
            return Err(VaultError::InvalidWalletMetadata);
        }
        if existing
            .iter()
            .any(|metadata| metadata.wallet_uuid == context_wallet_uuid)
        {
            return Err(VaultError::DuplicateWalletUuid);
        }

        let public_accounts = self.list_public_account_metadata_with_view(view)?;
        let candidates = self.passphrase_candidates(view, base_profile, &public_accounts)?;
        let spend_record = self.encrypted_record(&wallet_spend_record_key(base_profile_uuid))?;
        let spend_bundle = spend.decrypt_spend_bundle(base_profile_uuid, &spend_record)?;
        let mnemonic = Zeroizing::new(bip39_mnemonic_from_entropy(&spend_bundle.bip39_entropy)?);
        let seed = bip39_seed_from_mnemonic_zeroizing(&mnemonic, &passphrase)?;
        drop(passphrase);
        let evm_address = derive_public_evm_address_from_seed(&seed, 0)?;

        let mut matched = None;
        for candidate in candidates {
            let mut wallet = wallet_keys_from_seed(&seed, candidate.metadata.derivation_index)?;
            let railgun_matches = wallet_identity_matches(&wallet, &candidate.view_bundle);
            zeroize_wallet_keys(&mut wallet);
            let evm_matches = candidate
                .public_account
                .as_ref()
                .is_some_and(|account| account.address == evm_address);
            if railgun_matches != evm_matches {
                return Err(VaultError::InvalidSoftwareContextIdentity);
            }
            if railgun_matches {
                if matched.is_some() {
                    return Err(VaultError::DuplicateSoftwareContextIdentity);
                }
                matched = Some(candidate.metadata);
            }
        }

        if let Some(metadata) = matched {
            let binding = SoftwareSeedSessionBinding::new(
                base_profile_uuid,
                &metadata.wallet_uuid,
                vault_session_id,
            );
            let protected_seed_session =
                spend.seal_software_seed_session(binding, seed.as_ref())?;
            return Ok(CreateSoftwareContextResult::ExistingContext {
                metadata,
                protected_seed_session,
            });
        }

        let label = validate_wallet_label(label, &existing, None)?;
        let display_order = next_wallet_display_order(&existing)?;
        let mut chain_metadata = Vec::with_capacity(chains.len());
        let mut chain_keys = std::collections::BTreeSet::new();
        for chain in chains {
            let chain_key = (
                chain.chain_type,
                chain.chain_id,
                chain.contract.to_ascii_lowercase(),
            );
            if !chain_keys.insert(chain_key) {
                return Err(VaultError::DuplicateSoftwareContextChainInput);
            }
            let (start_block, last_scanned_block) = match intent {
                SoftwareContextSyncIntent::CreateNew => {
                    let safe_head = chain
                        .current_safe_head
                        .ok_or(VaultError::SoftwareContextSafeHeadUnavailable)?;
                    (
                        safe_head
                            .checked_add(1)
                            .ok_or(VaultError::SoftwareContextSafeHeadOverflow)?,
                        safe_head,
                    )
                }
                SoftwareContextSyncIntent::RecoverExisting => (
                    chain.deployment_block,
                    chain.deployment_block.saturating_sub(1),
                ),
            };
            chain_metadata.push(WalletChainMetadataBundle {
                wallet_chain_uuid: generate_opaque_id()?,
                wallet_uuid: context_wallet_uuid.to_owned(),
                chain_type: chain.chain_type,
                chain_id: chain.chain_id,
                contract: chain.contract.clone(),
                start_block,
                last_scanned_block,
                last_scanned_block_hash: None,
                poi_read_source: None,
            });
        }

        let mut wallet = wallet_keys_from_seed(&seed, railgun_derivation_index)?;
        let view_bundle =
            super::WalletViewBundle::from_wallet_keys(railgun_derivation_index, &wallet);
        zeroize_wallet_keys(&mut wallet);
        let metadata = WalletMetadataBundle {
            wallet_uuid: context_wallet_uuid.to_owned(),
            label,
            derivation_index: railgun_derivation_index,
            source: base_profile.source,
            status: WalletStatus::Active,
            display_order,
            hardware_descriptor: None,
            hardware_account: None,
            pending_create_new_chain_ids: std::collections::BTreeSet::new(),
            software_context: Some(WalletSoftwareContext::passphrase(base_profile_uuid)),
        };
        metadata.validate()?;
        let public_account =
            initial_derived_public_account_from_seed(context_wallet_uuid, &seed, &public_accounts)?;
        let binding = SoftwareSeedSessionBinding::new(
            base_profile_uuid,
            context_wallet_uuid,
            vault_session_id,
        );
        let protected_seed_session = spend.seal_software_seed_session(binding, seed.as_ref())?;

        let mut records = Vec::with_capacity(4 + chain_metadata.len() * 2);
        let view_record = view.encrypt_view_bundle(context_wallet_uuid, &view_bundle)?;
        records.push(view_record.to_record_entry(wallet_view_record_key(context_wallet_uuid))?);
        let metadata_record = view.encrypt_wallet_metadata(context_wallet_uuid, &metadata)?;
        records.push(
            metadata_record.to_record_entry(wallet_metadata_record_key(context_wallet_uuid))?,
        );
        records.push(public_account_metadata_record_entry(view, &public_account)?);
        for chain in &chain_metadata {
            let chain_record =
                view.encrypt_wallet_chain_metadata(&chain.wallet_chain_uuid, chain)?;
            records.push(
                chain_record
                    .to_record_entry(wallet_chain_metadata_record_key(&chain.wallet_chain_uuid))?,
            );
            records.push((
                wallet_chain_index_record_key(context_wallet_uuid, &chain.wallet_chain_uuid),
                rmp_serde::to_vec_named(&chain.chain_id)?,
            ));
        }
        records.push(wallet_chain_index_complete_record_entry(
            context_wallet_uuid,
        )?);
        self.db.put_desktop_wallet_vault_records(&records)?;

        Ok(CreateSoftwareContextResult::Created {
            metadata,
            public_account: Box::new(public_account),
            chain_metadata,
            protected_seed_session,
        })
    }

    pub fn match_software_context(
        &self,
        view: &ViewUnlock,
        base_profile: &WalletMetadataBundle,
        grant: &mut SpendGrant,
        passphrase: Zeroizing<String>,
        vault_session_id: VaultSessionId,
    ) -> Result<SoftwareContextMatch, VaultError> {
        let spend = grant.take_spend_unlock()?;
        self.match_software_context_with_spend_unlock(
            view,
            base_profile,
            &spend,
            passphrase,
            vault_session_id,
        )
    }

    pub fn match_software_context_with_spend_unlock(
        &self,
        view: &ViewUnlock,
        base_profile: &WalletMetadataBundle,
        spend: &SpendUnlock,
        passphrase: Zeroizing<String>,
        vault_session_id: VaultSessionId,
    ) -> Result<SoftwareContextMatch, VaultError> {
        validate_standard_base_profile(base_profile)?;
        let public_accounts = self.list_public_account_metadata_with_view(view)?;
        let candidates = self.passphrase_candidates(view, base_profile, &public_accounts)?;

        let spend_record =
            self.encrypted_record(&wallet_spend_record_key(&base_profile.wallet_uuid))?;
        let spend_bundle = spend.decrypt_spend_bundle(&base_profile.wallet_uuid, &spend_record)?;
        let mnemonic = Zeroizing::new(bip39_mnemonic_from_entropy(&spend_bundle.bip39_entropy)?);
        let seed = bip39_seed_from_mnemonic_zeroizing(&mnemonic, &passphrase)?;
        drop(passphrase);
        let evm_address = derive_public_evm_address_from_seed(&seed, 0)?;

        let mut matched = None;
        for candidate in candidates {
            let mut wallet = wallet_keys_from_seed(&seed, candidate.metadata.derivation_index)?;
            let railgun_matches = wallet_identity_matches(&wallet, &candidate.view_bundle);
            zeroize_wallet_keys(&mut wallet);
            let evm_matches = candidate
                .public_account
                .as_ref()
                .is_some_and(|account| account.address == evm_address);

            if railgun_matches != evm_matches {
                return Err(VaultError::InvalidSoftwareContextIdentity);
            }
            if !railgun_matches {
                continue;
            }
            if matched.is_some() {
                return Err(VaultError::DuplicateSoftwareContextIdentity);
            }
            matched = Some(candidate.metadata);
        }

        let Some(metadata) = matched else {
            return Ok(SoftwareContextMatch::Unknown);
        };
        let binding = SoftwareSeedSessionBinding::new(
            &base_profile.wallet_uuid,
            &metadata.wallet_uuid,
            vault_session_id,
        );
        let session = spend.seal_software_seed_session(binding, seed.as_ref())?;
        Ok(SoftwareContextMatch::Known {
            metadata: Box::new(metadata),
            session,
        })
    }

    pub fn match_software_context_with_spend_unlock_ref(
        &self,
        view: &ViewUnlock,
        base_profile: &WalletMetadataBundle,
        spend: &SpendUnlock,
        passphrase: &str,
        vault_session_id: VaultSessionId,
    ) -> Result<SoftwareContextMatch, VaultError> {
        self.match_software_context_with_spend_unlock(
            view,
            base_profile,
            spend,
            Zeroizing::new(passphrase.to_owned()),
            vault_session_id,
        )
    }

    pub fn match_software_context_with_view_unlock(
        &self,
        view: &ViewUnlock,
        base_profile_uuid: &str,
        grant: &mut SpendGrant,
        passphrase: Zeroizing<String>,
        vault_session_id: VaultSessionId,
    ) -> Result<SoftwareContextMatch, VaultError> {
        let base_profile = self.load_wallet_metadata_with_view(view, base_profile_uuid)?;
        self.match_software_context(view, &base_profile, grant, passphrase, vault_session_id)
    }

    fn passphrase_candidates(
        &self,
        view: &ViewUnlock,
        base_profile: &WalletMetadataBundle,
        public_accounts: &[PublicAccountMetadata],
    ) -> Result<Vec<CandidateContext>, VaultError> {
        let mut candidates = Vec::new();
        for wallet_id in self.list_wallet_ids()? {
            if wallet_id == base_profile.wallet_uuid {
                continue;
            }
            let Some(metadata_record) =
                self.encrypted_record_optional(&wallet_metadata_record_key(&wallet_id))?
            else {
                continue;
            };
            let metadata = view.decrypt_wallet_metadata(&wallet_id, &metadata_record)?;
            let Some(context) = metadata.software_context.as_ref() else {
                continue;
            };
            if context.kind != WalletSoftwareContextKind::Passphrase
                || context.base_profile_uuid != base_profile.wallet_uuid
            {
                continue;
            }

            let view_record = self
                .encrypted_record_optional(&wallet_view_record_key(&wallet_id))?
                .ok_or(VaultError::InvalidSoftwareContextIdentity)?;
            let view_bundle = view.decrypt_view_bundle(&wallet_id, &view_record)?;
            if view_bundle.derivation_index != metadata.derivation_index {
                return Err(VaultError::InvalidSoftwareContextIdentity);
            }

            let scoped_index_zero = public_accounts
                .iter()
                .filter(|account| {
                    matches!(
                        &account.scope,
                        PublicAccountScope::PrivateWallet { wallet_uuid }
                            if wallet_uuid == &wallet_id
                    ) && account.source == PublicAccountSource::Derived
                        && account.derivation_index == Some(0)
                })
                .cloned()
                .collect::<Vec<_>>();
            if scoped_index_zero.len() > 1 {
                return Err(VaultError::DuplicateSoftwareContextIdentity);
            }
            candidates.push(CandidateContext {
                metadata,
                view_bundle,
                public_account: scoped_index_zero.into_iter().next(),
            });
        }
        Ok(candidates)
    }
}

fn validate_standard_base_profile(metadata: &WalletMetadataBundle) -> Result<(), VaultError> {
    let Some(context) = metadata.software_context.as_ref() else {
        return Err(VaultError::InvalidWalletMetadata);
    };
    if metadata.source.is_hardware_derived()
        || context.kind != WalletSoftwareContextKind::Standard
        || context.base_profile_uuid != metadata.wallet_uuid
    {
        return Err(VaultError::InvalidWalletMetadata);
    }
    Ok(())
}

fn wallet_identity_matches(wallet: &WalletKeys, view_bundle: &super::WalletViewBundle) -> bool {
    wallet.spending_public_key.map(|value| value.to_be_bytes()) == view_bundle.spending_public_key
        && wallet.viewing.viewing_public_key == view_bundle.viewing_public_key
        && wallet.viewing.nullifying_key.to_be_bytes() == view_bundle.nullifying_key
        && wallet.viewing.master_public_key.to_be_bytes() == view_bundle.master_public_key
}
