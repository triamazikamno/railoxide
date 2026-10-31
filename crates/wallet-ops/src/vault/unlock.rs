use super::{
    BroadcasterPreferenceEntry, DecodedWalletMetadata, EncryptedRecord, HardwareProfileMetadata,
    HardwareViewAccessKey, HardwareWalletAccountIndexReservation, HmacKeyInit, HmacSha256, KEY_LEN,
    Mac, NonEvmAddressBookEntry, PrivateAddressBookEntry, ProtectedSoftwareSeedSession,
    PublicAccountMetadata, PublicAccountSecret, PublicAddressBookEntry, RecordKind,
    SOFTWARE_CONTEXT_SEED_LEN, SecretKey, SoftwareSeedSessionBinding, VaultError,
    WalletChainMetadataBundle, WalletConnectRelayIdentity, WalletConnectSessionRecord,
    WalletMetadataBundle, WalletMetadataWire, WalletPrivateRecordKind, WalletSpendBundle,
    WalletViewBundle, Zeroizing, decrypt_payload, decrypt_serialized, derive_context_key,
    encrypt_payload, encrypt_serialized,
};

pub struct ViewUnlock {
    pub(super) view_dek: SecretKey,
}

impl ViewUnlock {
    pub fn from_hardware_view_access_key(
        view_access_key: &HardwareViewAccessKey,
    ) -> Result<Self, VaultError> {
        SecretKey::from_zeroizing_vec(Zeroizing::new(view_access_key.expose_secret().to_vec()))
            .map(|view_dek| Self { view_dek })
    }

    #[must_use]
    pub const fn view_dek(&self) -> &SecretKey {
        &self.view_dek
    }

    #[must_use]
    pub fn clone_unlock(&self) -> Self {
        Self {
            view_dek: self.view_dek.clone_secret(),
        }
    }

    pub fn encrypt_record(
        &self,
        kind: RecordKind,
        record_id: &str,
        plaintext: &[u8],
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_payload(&self.view_dek, kind, record_id, plaintext)
    }

    pub fn decrypt_record(
        &self,
        kind: RecordKind,
        record_id: &str,
        record: &EncryptedRecord,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        decrypt_payload(&self.view_dek, kind, record_id, record)
    }

    pub fn encrypt_view_bundle(
        &self,
        wallet_id: &str,
        bundle: &WalletViewBundle,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::WalletViewBundle,
            wallet_id,
            bundle,
        )
    }

    pub fn decrypt_view_bundle(
        &self,
        wallet_id: &str,
        record: &EncryptedRecord,
    ) -> Result<WalletViewBundle, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::WalletViewBundle,
            wallet_id,
            record,
        )
    }

    pub fn encrypt_wallet_metadata(
        &self,
        wallet_uuid: &str,
        metadata: &WalletMetadataBundle,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::WalletMetadata,
            wallet_uuid,
            metadata,
        )
    }

    pub fn decrypt_wallet_metadata(
        &self,
        wallet_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<WalletMetadataBundle, VaultError> {
        Ok(self
            .decrypt_wallet_metadata_record(wallet_uuid, record)?
            .metadata)
    }

    pub(super) fn decrypt_wallet_metadata_record(
        &self,
        wallet_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<DecodedWalletMetadata, VaultError> {
        let wire: WalletMetadataWire = decrypt_serialized(
            &self.view_dek,
            RecordKind::WalletMetadata,
            wallet_uuid,
            record,
        )?;
        let missing_display_order = wire.display_order.is_none();
        let missing_lifecycle_fields =
            wire.source.is_none() || wire.status.is_none() || missing_display_order;
        let source = wire.source.unwrap_or_default();
        let missing_software_context = wire.software_context.is_none()
            && !source.is_hardware_derived()
            && wire.hardware_descriptor.is_none()
            && wire.hardware_account.is_none();
        let metadata = WalletMetadataBundle {
            wallet_uuid: wire.wallet_uuid,
            label: wire.label,
            derivation_index: wire.derivation_index,
            source,
            status: wire.status.unwrap_or_default(),
            display_order: wire.display_order.unwrap_or_default(),
            hardware_descriptor: wire.hardware_descriptor,
            hardware_account: wire.hardware_account,
            pending_create_new_chain_ids: wire.pending_create_new_chain_ids,
            software_context: wire.software_context,
        };
        if !missing_software_context {
            metadata.validate()?;
        }
        Ok(DecodedWalletMetadata {
            metadata,
            missing_lifecycle_fields,
            missing_display_order,
            missing_software_context,
        })
    }

    pub fn encrypt_wallet_chain_metadata(
        &self,
        wallet_chain_uuid: &str,
        metadata: &WalletChainMetadataBundle,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::WalletChainMetadata,
            wallet_chain_uuid,
            metadata,
        )
    }

    pub fn decrypt_wallet_chain_metadata(
        &self,
        wallet_chain_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<WalletChainMetadataBundle, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::WalletChainMetadata,
            wallet_chain_uuid,
            record,
        )
    }

    pub(super) fn encrypt_hardware_wallet_account_index_reservation(
        &self,
        reservation_uuid: &str,
        reservation: &HardwareWalletAccountIndexReservation,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::HardwareWalletAccountIndexReservation,
            reservation_uuid,
            reservation,
        )
    }

    pub(super) fn decrypt_hardware_wallet_account_index_reservation(
        &self,
        reservation_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<HardwareWalletAccountIndexReservation, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::HardwareWalletAccountIndexReservation,
            reservation_uuid,
            record,
        )
    }

    pub(super) fn encrypt_hardware_profile_metadata(
        &self,
        profile_id: &str,
        profile: &HardwareProfileMetadata,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::HardwareProfileMetadata,
            profile_id,
            profile,
        )
    }

    pub(super) fn decrypt_hardware_profile_metadata(
        &self,
        profile_id: &str,
        record: &EncryptedRecord,
    ) -> Result<HardwareProfileMetadata, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::HardwareProfileMetadata,
            profile_id,
            record,
        )
    }

    pub fn encrypt_public_account_metadata(
        &self,
        public_account_uuid: &str,
        metadata: &PublicAccountMetadata,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::PublicAccountMetadata,
            public_account_uuid,
            metadata,
        )
    }

    pub fn decrypt_public_account_metadata(
        &self,
        public_account_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<PublicAccountMetadata, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::PublicAccountMetadata,
            public_account_uuid,
            record,
        )
    }

    pub fn encrypt_private_address_book_entry(
        &self,
        entry_uuid: &str,
        entry: &PrivateAddressBookEntry,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::PrivateAddressBookEntry,
            entry_uuid,
            entry,
        )
    }

    pub fn decrypt_private_address_book_entry(
        &self,
        entry_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<PrivateAddressBookEntry, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::PrivateAddressBookEntry,
            entry_uuid,
            record,
        )
    }

    pub fn encrypt_public_address_book_entry(
        &self,
        entry_uuid: &str,
        entry: &PublicAddressBookEntry,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::PublicAddressBookEntry,
            entry_uuid,
            entry,
        )
    }

    pub fn decrypt_public_address_book_entry(
        &self,
        entry_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<PublicAddressBookEntry, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::PublicAddressBookEntry,
            entry_uuid,
            record,
        )
    }

    pub fn encrypt_non_evm_address_book_entry(
        &self,
        entry_uuid: &str,
        entry: &NonEvmAddressBookEntry,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::NonEvmAddressBookEntry,
            entry_uuid,
            entry,
        )
    }

    pub fn decrypt_non_evm_address_book_entry(
        &self,
        entry_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<NonEvmAddressBookEntry, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::NonEvmAddressBookEntry,
            entry_uuid,
            record,
        )
    }

    pub fn encrypt_broadcaster_preference_entry(
        &self,
        kind: RecordKind,
        entry_uuid: &str,
        entry: &BroadcasterPreferenceEntry,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(&self.view_dek, kind, entry_uuid, entry)
    }

    pub fn decrypt_broadcaster_preference_entry(
        &self,
        kind: RecordKind,
        entry_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<BroadcasterPreferenceEntry, VaultError> {
        decrypt_serialized(&self.view_dek, kind, entry_uuid, record)
    }

    pub fn encrypt_walletconnect_relay_identity(
        &self,
        wallet_uuid: &str,
        identity: &WalletConnectRelayIdentity,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::WalletConnectRelayIdentity,
            wallet_uuid,
            identity,
        )
    }

    pub fn decrypt_walletconnect_relay_identity(
        &self,
        wallet_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<WalletConnectRelayIdentity, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::WalletConnectRelayIdentity,
            wallet_uuid,
            record,
        )
    }

    pub fn encrypt_walletconnect_session(
        &self,
        session_uuid: &str,
        session: &WalletConnectSessionRecord,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.view_dek,
            RecordKind::WalletConnectSession,
            session_uuid,
            session,
        )
    }

    pub fn decrypt_walletconnect_session(
        &self,
        session_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<WalletConnectSessionRecord, VaultError> {
        decrypt_serialized(
            &self.view_dek,
            RecordKind::WalletConnectSession,
            session_uuid,
            record,
        )
    }

    pub fn derive_cache_keys(&self, wallet_chain_uuid: &str) -> Result<CacheKeys, VaultError> {
        Ok(CacheKeys {
            index: derive_context_key(
                &self.view_dek,
                b"cache-index",
                wallet_chain_uuid.as_bytes(),
            )?,
            data: derive_context_key(&self.view_dek, b"cache-data", wallet_chain_uuid.as_bytes())?,
        })
    }
}

pub struct SpendUnlock {
    pub(super) spend_dek: SecretKey,
}

impl SpendUnlock {
    #[must_use]
    pub const fn spend_dek(&self) -> &SecretKey {
        &self.spend_dek
    }

    pub fn encrypt_record(
        &self,
        kind: RecordKind,
        record_id: &str,
        plaintext: &[u8],
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_payload(&self.spend_dek, kind, record_id, plaintext)
    }

    pub fn decrypt_record(
        &self,
        kind: RecordKind,
        record_id: &str,
        record: &EncryptedRecord,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        decrypt_payload(&self.spend_dek, kind, record_id, record)
    }

    pub fn seal_software_seed_session(
        &self,
        binding: SoftwareSeedSessionBinding,
        seed: &[u8],
    ) -> Result<ProtectedSoftwareSeedSession, VaultError> {
        if seed.len() != SOFTWARE_CONTEXT_SEED_LEN {
            return Err(VaultError::InvalidSoftwareSeedLength);
        }
        let encrypted_seed = encrypt_payload(
            &self.spend_dek,
            RecordKind::SoftwareContextSeed,
            &binding.record_id(),
            seed,
        )?;
        Ok(ProtectedSoftwareSeedSession::from_parts(
            binding,
            encrypted_seed,
        ))
    }

    pub fn encrypt_spend_bundle(
        &self,
        wallet_id: &str,
        bundle: &WalletSpendBundle,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.spend_dek,
            RecordKind::WalletSpendBundle,
            wallet_id,
            bundle,
        )
    }

    pub fn decrypt_spend_bundle(
        &self,
        wallet_id: &str,
        record: &EncryptedRecord,
    ) -> Result<WalletSpendBundle, VaultError> {
        decrypt_serialized(
            &self.spend_dek,
            RecordKind::WalletSpendBundle,
            wallet_id,
            record,
        )
    }

    pub fn encrypt_public_account_secret(
        &self,
        public_account_uuid: &str,
        secret: &PublicAccountSecret,
    ) -> Result<EncryptedRecord, VaultError> {
        encrypt_serialized(
            &self.spend_dek,
            RecordKind::PublicAccountSecret,
            public_account_uuid,
            secret,
        )
    }

    pub fn decrypt_public_account_secret(
        &self,
        public_account_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<PublicAccountSecret, VaultError> {
        decrypt_serialized(
            &self.spend_dek,
            RecordKind::PublicAccountSecret,
            public_account_uuid,
            record,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendGrantPolicy {
    OneUse,
}

pub struct SpendGrant {
    pub(super) policy: SpendGrantPolicy,
    pub(super) spend: Option<SpendUnlock>,
}

pub struct CacheKeys {
    pub(super) index: SecretKey,
    pub(super) data: SecretKey,
}

impl CacheKeys {
    const fn private_record_kind(kind: WalletPrivateRecordKind) -> RecordKind {
        match kind {
            WalletPrivateRecordKind::PendingOutputPoiContext => RecordKind::PendingOutputPoiContext,
            WalletPrivateRecordKind::OutputPoiRecovery => RecordKind::OutputPoiRecovery,
            WalletPrivateRecordKind::SenderTransactionCandidate => {
                RecordKind::SenderTransactionCandidate
            }
        }
    }

    const fn private_record_domain(kind: WalletPrivateRecordKind) -> &'static [u8] {
        match kind {
            WalletPrivateRecordKind::PendingOutputPoiContext => b"pending-output-poi-context",
            WalletPrivateRecordKind::OutputPoiRecovery => b"output-poi-recovery",
            WalletPrivateRecordKind::SenderTransactionCandidate => b"sender-transaction-candidate",
        }
    }

    /// # Panics
    ///
    /// Panics only if HMAC rejects a fixed-size SHA-256 key, which should be
    /// unreachable for the selected MAC implementation.
    #[must_use]
    pub fn row_id(&self, tree: u32, position: u64, stable_utxo_identity: &[u8]) -> [u8; KEY_LEN] {
        let mut mac = HmacSha256::new_from_slice(self.index.expose_secret())
            .expect("HMAC accepts any key length");
        mac.update(b"wallet-cache-row:v1");
        mac.update(&tree.to_be_bytes());
        mac.update(&position.to_be_bytes());
        mac.update(stable_utxo_identity);
        mac.finalize().into_bytes().into()
    }

    #[must_use]
    pub fn row_record_id(row_id: &[u8; KEY_LEN]) -> String {
        alloy::hex::encode(row_id)
    }

    pub fn encrypt_row(
        &self,
        row_id: &[u8; KEY_LEN],
        plaintext: &[u8],
    ) -> Result<EncryptedRecord, VaultError> {
        let record_id = Self::row_record_id(row_id);
        encrypt_payload(
            &self.data,
            RecordKind::WalletCacheRow,
            &record_id,
            plaintext,
        )
    }

    pub fn decrypt_row(
        &self,
        row_id: &[u8; KEY_LEN],
        record: &EncryptedRecord,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let record_id = Self::row_record_id(row_id);
        decrypt_payload(&self.data, RecordKind::WalletCacheRow, &record_id, record)
    }

    /// # Panics
    ///
    /// Panics only if HMAC rejects a fixed-size SHA-256 key, which should be
    /// unreachable for the selected MAC implementation.
    #[must_use]
    pub fn private_row_id(
        &self,
        kind: WalletPrivateRecordKind,
        wallet_chain_namespace: &str,
        semantic_id: &[u8],
    ) -> [u8; KEY_LEN] {
        let mut mac = HmacSha256::new_from_slice(self.index.expose_secret())
            .expect("HMAC accepts any key length");
        mac.update(b"wallet-private-row:v2");
        mac.update(Self::private_record_domain(kind));
        mac.update(&(wallet_chain_namespace.len() as u64).to_be_bytes());
        mac.update(wallet_chain_namespace.as_bytes());
        mac.update(&(semantic_id.len() as u64).to_be_bytes());
        mac.update(semantic_id);
        mac.finalize().into_bytes().into()
    }

    pub fn encrypt_private_row(
        &self,
        kind: WalletPrivateRecordKind,
        wallet_chain_namespace: &str,
        row_id: &[u8; KEY_LEN],
        plaintext: &[u8],
    ) -> Result<EncryptedRecord, VaultError> {
        let record_id = format!("{wallet_chain_namespace}:{}", Self::row_record_id(row_id));
        encrypt_payload(
            &self.data,
            Self::private_record_kind(kind),
            &record_id,
            plaintext,
        )
    }

    pub fn decrypt_private_row(
        &self,
        kind: WalletPrivateRecordKind,
        wallet_chain_namespace: &str,
        row_id: &[u8; KEY_LEN],
        record: &EncryptedRecord,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let record_id = format!("{wallet_chain_namespace}:{}", Self::row_record_id(row_id));
        decrypt_payload(
            &self.data,
            Self::private_record_kind(kind),
            &record_id,
            record,
        )
    }
}

impl SpendGrant {
    #[must_use]
    pub const fn one_use(spend: SpendUnlock) -> Self {
        Self {
            policy: SpendGrantPolicy::OneUse,
            spend: Some(spend),
        }
    }

    #[must_use]
    pub const fn policy(&self) -> SpendGrantPolicy {
        self.policy
    }

    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.spend.is_some()
    }

    pub fn spend_unlock(&self) -> Result<&SpendUnlock, VaultError> {
        self.spend.as_ref().ok_or(VaultError::InvalidSpendGrant)
    }

    pub fn take_spend_unlock(&mut self) -> Result<SpendUnlock, VaultError> {
        self.spend.take().ok_or(VaultError::InvalidSpendGrant)
    }

    pub fn invalidate(&mut self) {
        self.spend.take();
    }
}
