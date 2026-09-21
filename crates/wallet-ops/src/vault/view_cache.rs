use super::{
    Arc, CacheKeys, DbStore, DeserializeOwned, DesktopEncryptedWalletCacheStore,
    DesktopViewSession, EncryptedRecord, HardwareProfileSession, Instant, KEY_LEN, Mutex,
    RailgunError, Serialize, U256, VaultError, ViewUnlock, ViewingKeyData, WalletCacheError,
    WalletCacheKey, WalletChainMetadataBundle, WalletMeta, WalletUtxo, WalletViewBundle,
    deserialize_wallet_utxo, serialize_wallet_utxo, vault_error_from_wallet_cache,
    wallet_cache_counts, wallet_utxo_stable_identity,
};
use alloy::primitives::FixedBytes;
use local_db::{
    DbError, OpaqueWalletPrivateRow, OpaqueWalletPrivateRowMutation, OutputPoiRecoveryRecord,
    PendingOutputPoiContextRecord, WalletMetaMutation, WalletPrivateNamespaceId,
    WalletPrivateRecordKind, WalletPrivateStateBatch, WalletUtxoRowMutation,
};
use sync_service::types::{WalletCheckpointMutation, WalletUtxoMutation};
use sync_service::{
    SenderTransactionCandidate, WalletCacheStore, sender_transaction_candidate_rewind_ids,
};

impl DesktopViewSession {
    #[must_use]
    pub fn from_bundle(wallet_id: String, bundle: &WalletViewBundle, view: ViewUnlock) -> Self {
        let private_view = view.clone_unlock();
        Self {
            session_identity: Arc::new(()),
            wallet_id,
            derivation_index: bundle.derivation_index,
            spending_public_key: bundle.spending_public_key(),
            scan_keys: bundle.scan_keys(),
            view,
            private_view,
            hardware_profile_session: None,
        }
    }

    #[must_use]
    pub fn from_hardware_bundle(
        wallet_id: String,
        bundle: &WalletViewBundle,
        view: ViewUnlock,
        private_view: ViewUnlock,
        hardware_profile_session: HardwareProfileSession,
    ) -> Self {
        Self {
            session_identity: Arc::new(()),
            wallet_id,
            derivation_index: bundle.derivation_index,
            spending_public_key: bundle.spending_public_key(),
            scan_keys: bundle.scan_keys(),
            view,
            private_view,
            hardware_profile_session: Some(hardware_profile_session),
        }
    }

    #[must_use]
    pub fn wallet_id(&self) -> &str {
        &self.wallet_id
    }

    #[must_use]
    pub const fn derivation_index(&self) -> u32 {
        self.derivation_index
    }

    #[must_use]
    pub const fn scan_keys(&self) -> ViewingKeyData {
        self.scan_keys
    }

    #[must_use]
    pub const fn spending_public_key(&self) -> [U256; 2] {
        self.spending_public_key
    }

    #[must_use]
    pub const fn hardware_profile_session(&self) -> Option<&HardwareProfileSession> {
        self.hardware_profile_session.as_ref()
    }

    #[must_use]
    pub fn clone_vault_view_unlock(&self) -> ViewUnlock {
        self.view.clone_unlock()
    }

    #[must_use]
    pub fn clone_with_hardware_profile_session(
        &self,
        hardware_profile_session: HardwareProfileSession,
    ) -> Self {
        Self {
            session_identity: Arc::clone(&self.session_identity),
            wallet_id: self.wallet_id.clone(),
            derivation_index: self.derivation_index,
            spending_public_key: self.spending_public_key,
            scan_keys: self.scan_keys,
            view: self.view.clone_unlock(),
            private_view: self.private_view.clone_unlock(),
            hardware_profile_session: Some(hardware_profile_session),
        }
    }

    /// Metadata refresh preserves this lifecycle; reopening the wallet does not.
    #[must_use]
    pub fn is_same_wallet_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session_identity, &other.session_identity)
            && match (
                &self.hardware_profile_session,
                &other.hardware_profile_session,
            ) {
                (None, None) => true,
                (Some(current), Some(other)) => {
                    current.profile_id == other.profile_id
                        && current.device_kind == other.device_kind
                        && current.binding == other.binding
                }
                _ => false,
            }
    }

    pub fn receive_address(&self) -> Result<String, RailgunError> {
        Ok(self.scan_keys.derive_address(None)?.to_string())
    }

    pub fn encrypt_wallet_chain_metadata(
        &self,
        wallet_chain_uuid: &str,
        metadata: &WalletChainMetadataBundle,
    ) -> Result<EncryptedRecord, VaultError> {
        self.private_view
            .encrypt_wallet_chain_metadata(wallet_chain_uuid, metadata)
    }

    pub fn decrypt_wallet_chain_metadata(
        &self,
        wallet_chain_uuid: &str,
        record: &EncryptedRecord,
    ) -> Result<WalletChainMetadataBundle, VaultError> {
        self.private_view
            .decrypt_wallet_chain_metadata(wallet_chain_uuid, record)
    }

    pub fn derive_cache_keys(&self, wallet_chain_uuid: &str) -> Result<CacheKeys, VaultError> {
        self.private_view.derive_cache_keys(wallet_chain_uuid)
    }
}

impl DesktopEncryptedWalletCacheStore {
    pub(super) fn for_chain_cache_repair(
        db: Arc<DbStore>,
        view_session: &DesktopViewSession,
        metadata: WalletChainMetadataBundle,
    ) -> Result<Self, VaultError> {
        if metadata.wallet_uuid != view_session.wallet_id() {
            return Err(VaultError::UnlockFailed);
        }
        let cache_keys = view_session.derive_cache_keys(&metadata.wallet_chain_uuid)?;
        Ok(Self {
            db,
            metadata: Mutex::new(metadata),
            cache_keys,
        })
    }

    pub fn new(
        db: Arc<DbStore>,
        view_session: &Arc<DesktopViewSession>,
        metadata: WalletChainMetadataBundle,
    ) -> Result<Self, VaultError> {
        let chain_id = metadata.chain_id;
        let hardware = view_session.hardware_profile_session().is_some();
        if metadata.wallet_uuid != view_session.wallet_id() {
            tracing::error!(
                chain_id,
                hardware,
                stage = "validate_wallet_owner",
                "encrypted wallet cache initialization failed"
            );
            return Err(VaultError::UnlockFailed);
        }
        let cache_keys = view_session
            .derive_cache_keys(&metadata.wallet_chain_uuid)
            .inspect_err(|error| {
                tracing::error!(
                    chain_id,
                    hardware,
                    stage = "derive_cache_keys",
                    error = %error,
                    "encrypted wallet cache initialization failed"
                );
            })?;
        let wallet_cache_key = metadata
            .wallet_chain_uuid
            .parse::<WalletCacheKey>()
            .inspect_err(|error| {
                tracing::error!(
                    chain_id,
                    hardware,
                    stage = "parse_canonical_namespace",
                    error = %error,
                    "encrypted wallet cache initialization failed"
                );
            })?;
        let canonical_namespace = WalletPrivateNamespaceId::new(chain_id, wallet_cache_key);
        let legacy_namespace = metadata
            .wallet_uuid
            .parse::<WalletCacheKey>()
            .ok()
            .map(|wallet_id| WalletPrivateNamespaceId::new(metadata.chain_id, wallet_id));
        let cache = Self {
            db,
            metadata: Mutex::new(metadata),
            cache_keys,
        };
        cache
            .canonicalize_wallet_private_storage(
                &canonical_namespace,
                legacy_namespace
                    .as_ref()
                    .filter(|legacy| *legacy != &canonical_namespace),
            )
            .inspect_err(|error| {
                tracing::error!(
                    chain_id,
                    hardware,
                    stage = "canonicalize_wallet_private_storage",
                    error = %error,
                    "encrypted wallet cache initialization failed"
                );
            })?;
        Ok(cache)
    }

    fn wallet_chain_uuid(&self) -> Result<String, WalletCacheError> {
        Ok(self
            .metadata
            .lock()
            .map_err(|_| WalletCacheError::Crypto)?
            .wallet_chain_uuid
            .clone())
    }

    fn private_namespace(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
    ) -> Result<WalletPrivateNamespaceId, WalletCacheError> {
        let metadata = self.metadata.lock().map_err(|_| WalletCacheError::Crypto)?;
        if metadata.chain_id != chain_id || metadata.wallet_chain_uuid != wallet_id.as_str() {
            return Err(DbError::InvalidWalletPrivateCommitNamespace {
                expected_chain_id: metadata.chain_id,
                expected_wallet_id: metadata.wallet_chain_uuid.clone(),
                actual_chain_id: chain_id,
                actual_wallet_id: wallet_id.to_string(),
            }
            .into());
        }
        Ok(WalletPrivateNamespaceId::new(chain_id, wallet_id.clone()))
    }

    pub(super) fn sender_transaction_candidate_rewind_row_ids(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
        from_block: u64,
    ) -> Result<Vec<Vec<u8>>, VaultError> {
        let namespace = self
            .private_namespace(chain_id, wallet_id)
            .map_err(vault_error_from_wallet_cache)?;
        let candidates = self
            .list_sender_transaction_candidates(chain_id, wallet_id)
            .map_err(vault_error_from_wallet_cache)?;
        let semantic_ids = sender_transaction_candidate_rewind_ids(&candidates, from_block)
            .map_err(|_| VaultError::Decrypt)?;
        Ok(semantic_ids
            .iter()
            .map(|semantic_id| {
                self.cache_keys
                    .private_row_id(
                        WalletPrivateRecordKind::SenderTransactionCandidate,
                        namespace.wallet_id.as_str(),
                        semantic_id.as_slice(),
                    )
                    .to_vec()
            })
            .collect())
    }

    pub(super) fn encrypted_private_row<T: Serialize>(
        &self,
        namespace: &WalletPrivateNamespaceId,
        kind: WalletPrivateRecordKind,
        semantic_id: &[u8],
        value: &T,
    ) -> Result<OpaqueWalletPrivateRow, VaultError> {
        let row_id =
            self.cache_keys
                .private_row_id(kind, namespace.wallet_id.as_str(), semantic_id);
        let plaintext = rmp_serde::to_vec_named(value)?;
        let encrypted = self.cache_keys.encrypt_private_row(
            kind,
            namespace.wallet_id.as_str(),
            &row_id,
            &plaintext,
        )?;
        Ok(OpaqueWalletPrivateRow {
            row_id: row_id.to_vec(),
            payload: rmp_serde::to_vec_named(&encrypted)?,
        })
    }

    fn decrypted_private_row<T: DeserializeOwned>(
        &self,
        namespace: &WalletPrivateNamespaceId,
        kind: WalletPrivateRecordKind,
        row: &OpaqueWalletPrivateRow,
    ) -> Result<T, VaultError> {
        let row_id: [u8; KEY_LEN] = row
            .row_id
            .as_slice()
            .try_into()
            .map_err(|_| VaultError::Decrypt)?;
        let encrypted: EncryptedRecord = rmp_serde::from_slice(&row.payload)?;
        let plaintext = self.cache_keys.decrypt_private_row(
            kind,
            namespace.wallet_id.as_str(),
            &row_id,
            &encrypted,
        )?;
        Ok(rmp_serde::from_slice(&plaintext)?)
    }

    pub(super) fn pending_output_row(
        &self,
        namespace: &WalletPrivateNamespaceId,
        record: &PendingOutputPoiContextRecord,
    ) -> Result<OpaqueWalletPrivateRow, VaultError> {
        if record.chain_id != namespace.chain_id || record.wallet_id != namespace.wallet_id.as_str()
        {
            return Err(VaultError::Decrypt);
        }
        self.encrypted_private_row(
            namespace,
            WalletPrivateRecordKind::PendingOutputPoiContext,
            record.output_commitment.as_slice(),
            record,
        )
    }

    pub(super) fn output_recovery_row(
        &self,
        namespace: &WalletPrivateNamespaceId,
        record: &OutputPoiRecoveryRecord,
    ) -> Result<OpaqueWalletPrivateRow, VaultError> {
        if record.chain_id != namespace.chain_id || record.wallet_id != namespace.wallet_id.as_str()
        {
            return Err(VaultError::Decrypt);
        }
        self.encrypted_private_row(
            namespace,
            WalletPrivateRecordKind::OutputPoiRecovery,
            record.output_commitment.as_slice(),
            record,
        )
    }

    pub(super) fn sender_transaction_candidate_row(
        &self,
        namespace: &WalletPrivateNamespaceId,
        candidate: &SenderTransactionCandidate,
    ) -> Result<OpaqueWalletPrivateRow, VaultError> {
        if candidate.chain_id != namespace.chain_id
            || candidate.wallet_id.as_str() != namespace.wallet_id.as_str()
        {
            return Err(VaultError::Decrypt);
        }
        let semantic_id = candidate.semantic_id();
        let row_id = self.cache_keys.private_row_id(
            WalletPrivateRecordKind::SenderTransactionCandidate,
            namespace.wallet_id.as_str(),
            semantic_id.as_slice(),
        );
        let plaintext = candidate.encode().map_err(|_| VaultError::Decrypt)?;
        let encrypted = self.cache_keys.encrypt_private_row(
            WalletPrivateRecordKind::SenderTransactionCandidate,
            namespace.wallet_id.as_str(),
            &row_id,
            &plaintext,
        )?;
        Ok(OpaqueWalletPrivateRow {
            row_id: row_id.to_vec(),
            payload: rmp_serde::to_vec_named(&encrypted)?,
        })
    }

    pub(super) fn decode_pending_output_row(
        &self,
        namespace: &WalletPrivateNamespaceId,
        row: &OpaqueWalletPrivateRow,
    ) -> Result<PendingOutputPoiContextRecord, VaultError> {
        let record: PendingOutputPoiContextRecord = self.decrypted_private_row(
            namespace,
            WalletPrivateRecordKind::PendingOutputPoiContext,
            row,
        )?;
        if record.chain_id != namespace.chain_id || record.wallet_id != namespace.wallet_id.as_str()
        {
            return Err(VaultError::Decrypt);
        }
        let expected_row_id = self.cache_keys.private_row_id(
            WalletPrivateRecordKind::PendingOutputPoiContext,
            namespace.wallet_id.as_str(),
            record.output_commitment.as_slice(),
        );
        if expected_row_id.as_slice() != row.row_id.as_slice() {
            return Err(VaultError::Decrypt);
        }
        Ok(record)
    }

    pub(super) fn decode_output_recovery_row(
        &self,
        namespace: &WalletPrivateNamespaceId,
        row: &OpaqueWalletPrivateRow,
    ) -> Result<OutputPoiRecoveryRecord, VaultError> {
        let record: OutputPoiRecoveryRecord =
            self.decrypted_private_row(namespace, WalletPrivateRecordKind::OutputPoiRecovery, row)?;
        if record.chain_id != namespace.chain_id || record.wallet_id != namespace.wallet_id.as_str()
        {
            return Err(VaultError::Decrypt);
        }
        let expected_row_id = self.cache_keys.private_row_id(
            WalletPrivateRecordKind::OutputPoiRecovery,
            namespace.wallet_id.as_str(),
            record.output_commitment.as_slice(),
        );
        if expected_row_id.as_slice() != row.row_id.as_slice() {
            return Err(VaultError::Decrypt);
        }
        Ok(record)
    }

    pub(super) fn decode_sender_transaction_candidate_row(
        &self,
        namespace: &WalletPrivateNamespaceId,
        row: &OpaqueWalletPrivateRow,
    ) -> Result<SenderTransactionCandidate, VaultError> {
        let row_id: [u8; KEY_LEN] = row
            .row_id
            .as_slice()
            .try_into()
            .map_err(|_| VaultError::Decrypt)?;
        let encrypted: EncryptedRecord = rmp_serde::from_slice(&row.payload)?;
        let plaintext = self.cache_keys.decrypt_private_row(
            WalletPrivateRecordKind::SenderTransactionCandidate,
            namespace.wallet_id.as_str(),
            &row_id,
            &encrypted,
        )?;
        let candidate =
            SenderTransactionCandidate::decode(&plaintext).map_err(|_| VaultError::Decrypt)?;
        if candidate.chain_id != namespace.chain_id
            || candidate.wallet_id.as_str() != namespace.wallet_id.as_str()
        {
            return Err(VaultError::Decrypt);
        }
        let semantic_id = candidate.semantic_id();
        let expected_row_id = self.cache_keys.private_row_id(
            WalletPrivateRecordKind::SenderTransactionCandidate,
            namespace.wallet_id.as_str(),
            semantic_id.as_slice(),
        );
        if expected_row_id != row_id {
            return Err(VaultError::Decrypt);
        }
        Ok(candidate)
    }

    fn encrypted_wallet_utxo_entries(
        &self,
        utxos: &[WalletUtxo],
    ) -> Result<Vec<(String, Vec<u8>)>, WalletCacheError> {
        let mut entries = Vec::with_capacity(utxos.len());
        for utxo in utxos {
            let stable_identity = wallet_utxo_stable_identity(utxo);
            let row_id =
                self.cache_keys
                    .row_id(utxo.utxo.tree, utxo.utxo.position, &stable_identity);
            let plaintext = serialize_wallet_utxo(utxo)?;
            let record = self
                .cache_keys
                .encrypt_row(&row_id, &plaintext)
                .map_err(|_| WalletCacheError::Crypto)?;
            entries.push((
                alloy::hex::encode(row_id),
                rmp_serde::to_vec_named(&record)?,
            ));
        }
        Ok(entries)
    }

    #[cfg(test)]
    pub(crate) fn replace_wallet_cache_atomically_for_test(
        &self,
        wallet_id: &WalletCacheKey,
        utxos: &[WalletUtxo],
        last_scanned_block: u64,
        last_scanned_block_hash: Option<[u8; KEY_LEN]>,
    ) -> Result<(), WalletCacheError> {
        let entries = self.encrypted_wallet_utxo_entries(utxos)?;
        let meta = WalletMeta {
            last_scanned_block,
            updated_at: 0,
            last_scanned_block_hash,
        };
        let chain_id = self
            .metadata
            .lock()
            .map_err(|_| WalletCacheError::Crypto)?
            .chain_id;
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        self.db
            .batch_commit_wallet_private_state(&WalletPrivateStateBatch {
                namespace: &namespace,
                utxos: WalletUtxoRowMutation::Replace(&entries),
                metadata: WalletMetaMutation::Set(&meta),
                sync_actor_state: None,
                pending_output_contexts: OpaqueWalletPrivateRowMutation::default(),
                output_poi_recoveries: OpaqueWalletPrivateRowMutation::default(),
                sender_transaction_candidates: OpaqueWalletPrivateRowMutation::default(),
            })?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn commit_poi_workflow_for_test(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
        pending_output_contexts: &[PendingOutputPoiContextRecord],
        output_poi_recoveries: &[OutputPoiRecoveryRecord],
    ) -> Result<(), WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        let pending_output_contexts = pending_output_contexts
            .iter()
            .map(|record| self.pending_output_row(&namespace, record))
            .collect::<Result<Vec<_>, VaultError>>()
            .map_err(|_| WalletCacheError::Crypto)?;
        let output_poi_recoveries = output_poi_recoveries
            .iter()
            .map(|record| self.output_recovery_row(&namespace, record))
            .collect::<Result<Vec<_>, VaultError>>()
            .map_err(|_| WalletCacheError::Crypto)?;
        self.db
            .batch_commit_wallet_private_state(&WalletPrivateStateBatch {
                namespace: &namespace,
                utxos: WalletUtxoRowMutation::Preserve,
                metadata: WalletMetaMutation::Preserve,
                sync_actor_state: None,
                pending_output_contexts: OpaqueWalletPrivateRowMutation {
                    updates: &pending_output_contexts,
                    deletes: &[],
                },
                output_poi_recoveries: OpaqueWalletPrivateRowMutation {
                    updates: &output_poi_recoveries,
                    deletes: &[],
                },
                sender_transaction_candidates: OpaqueWalletPrivateRowMutation::default(),
            })?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn delete_poi_workflow_for_test(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
        pending_output_contexts: &[FixedBytes<32>],
        output_poi_recoveries: &[FixedBytes<32>],
    ) -> Result<(), WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        let pending_output_contexts = pending_output_contexts
            .iter()
            .map(|commitment| {
                self.cache_keys
                    .private_row_id(
                        WalletPrivateRecordKind::PendingOutputPoiContext,
                        namespace.wallet_id.as_str(),
                        commitment.as_slice(),
                    )
                    .to_vec()
            })
            .collect::<Vec<_>>();
        let output_poi_recoveries = output_poi_recoveries
            .iter()
            .map(|commitment| {
                self.cache_keys
                    .private_row_id(
                        WalletPrivateRecordKind::OutputPoiRecovery,
                        namespace.wallet_id.as_str(),
                        commitment.as_slice(),
                    )
                    .to_vec()
            })
            .collect::<Vec<_>>();
        self.db
            .batch_commit_wallet_private_state(&WalletPrivateStateBatch {
                namespace: &namespace,
                utxos: WalletUtxoRowMutation::Preserve,
                metadata: WalletMetaMutation::Preserve,
                sync_actor_state: None,
                pending_output_contexts: OpaqueWalletPrivateRowMutation {
                    updates: &[],
                    deletes: &pending_output_contexts,
                },
                output_poi_recoveries: OpaqueWalletPrivateRowMutation {
                    updates: &[],
                    deletes: &output_poi_recoveries,
                },
                sender_transaction_candidates: OpaqueWalletPrivateRowMutation::default(),
            })?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn commit_sender_transaction_candidates_for_test(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
        updates: &[SenderTransactionCandidate],
        deletes: &[FixedBytes<32>],
    ) -> Result<(), WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        let updates = updates
            .iter()
            .map(|candidate| self.sender_transaction_candidate_row(&namespace, candidate))
            .collect::<Result<Vec<_>, VaultError>>()
            .map_err(|_| WalletCacheError::Crypto)?;
        let deletes = deletes
            .iter()
            .map(|transaction_hash| {
                self.cache_keys
                    .private_row_id(
                        WalletPrivateRecordKind::SenderTransactionCandidate,
                        namespace.wallet_id.as_str(),
                        transaction_hash.as_slice(),
                    )
                    .to_vec()
            })
            .collect::<Vec<_>>();
        self.db
            .batch_commit_wallet_private_state(&WalletPrivateStateBatch {
                namespace: &namespace,
                utxos: WalletUtxoRowMutation::Preserve,
                metadata: WalletMetaMutation::Preserve,
                sync_actor_state: None,
                pending_output_contexts: OpaqueWalletPrivateRowMutation::default(),
                output_poi_recoveries: OpaqueWalletPrivateRowMutation::default(),
                sender_transaction_candidates: OpaqueWalletPrivateRowMutation {
                    updates: &updates,
                    deletes: &deletes,
                },
            })?;
        Ok(())
    }
}

impl sync_service::WalletCacheStore for DesktopEncryptedWalletCacheStore {
    fn commit_wallet_private_state(
        &self,
        commit: sync_service::types::WalletPrivateCommit<'_>,
    ) -> Result<(), WalletCacheError> {
        commit.validate_namespace()?;
        let started = Instant::now();
        let encode_started = Instant::now();
        let (utxo_entries, wallet_counts) = match commit.utxo_mutation() {
            WalletUtxoMutation::Preserve => (None, None),
            WalletUtxoMutation::Replace(utxos) => (
                Some(self.encrypted_wallet_utxo_entries(utxos)?),
                Some(wallet_cache_counts(utxos)),
            ),
        };
        let wallet_meta = match commit.checkpoint_mutation() {
            WalletCheckpointMutation::Preserve => None,
            WalletCheckpointMutation::Set {
                last_scanned_block,
                last_scanned_block_hash,
            } => Some(WalletMeta {
                last_scanned_block,
                updated_at: 0,
                last_scanned_block_hash,
            }),
        };
        let encode_elapsed_ms = encode_started.elapsed().as_millis();

        let namespace = self.private_namespace(commit.chain_id(), commit.wallet_id())?;
        let mut metadata = if wallet_meta.is_some() {
            Some(self.metadata.lock().map_err(|_| WalletCacheError::Crypto)?)
        } else {
            None
        };
        let db_started = Instant::now();
        let pending_output_context_updates = commit
            .pending_output_context_updates()
            .iter()
            .map(|record| self.pending_output_row(&namespace, record))
            .collect::<Result<Vec<_>, VaultError>>()
            .map_err(|_| WalletCacheError::Crypto)?;
        let pending_output_context_deletes = commit
            .pending_output_context_deletes()
            .iter()
            .map(|commitment| {
                self.cache_keys
                    .private_row_id(
                        WalletPrivateRecordKind::PendingOutputPoiContext,
                        namespace.wallet_id.as_str(),
                        commitment.as_slice(),
                    )
                    .to_vec()
            })
            .collect::<Vec<_>>();
        let output_poi_recovery_updates = commit
            .output_poi_recovery_updates()
            .iter()
            .map(|record| self.output_recovery_row(&namespace, record))
            .collect::<Result<Vec<_>, VaultError>>()
            .map_err(|_| WalletCacheError::Crypto)?;
        let output_poi_recovery_deletes = commit
            .output_poi_recovery_deletes()
            .iter()
            .map(|commitment| {
                self.cache_keys
                    .private_row_id(
                        WalletPrivateRecordKind::OutputPoiRecovery,
                        namespace.wallet_id.as_str(),
                        commitment.as_slice(),
                    )
                    .to_vec()
            })
            .collect::<Vec<_>>();
        let sender_transaction_candidate_updates = commit
            .sender_transaction_candidate_updates()
            .iter()
            .map(|candidate| self.sender_transaction_candidate_row(&namespace, candidate))
            .collect::<Result<Vec<_>, VaultError>>()
            .map_err(|_| WalletCacheError::Crypto)?;
        let sender_transaction_candidate_deletes = commit
            .sender_transaction_candidate_deletes()
            .iter()
            .map(|transaction_hash| {
                self.cache_keys
                    .private_row_id(
                        WalletPrivateRecordKind::SenderTransactionCandidate,
                        namespace.wallet_id.as_str(),
                        transaction_hash.as_slice(),
                    )
                    .to_vec()
            })
            .collect::<Vec<_>>();
        self.db
            .batch_commit_wallet_private_state(&WalletPrivateStateBatch {
                namespace: &namespace,
                utxos: utxo_entries.as_deref().map_or(
                    WalletUtxoRowMutation::Preserve,
                    WalletUtxoRowMutation::Replace,
                ),
                metadata: wallet_meta
                    .as_ref()
                    .map_or(WalletMetaMutation::Preserve, WalletMetaMutation::Set),
                sync_actor_state: commit.sync_actor_state(),
                pending_output_contexts: OpaqueWalletPrivateRowMutation {
                    updates: &pending_output_context_updates,
                    deletes: &pending_output_context_deletes,
                },
                output_poi_recoveries: OpaqueWalletPrivateRowMutation {
                    updates: &output_poi_recovery_updates,
                    deletes: &output_poi_recovery_deletes,
                },
                sender_transaction_candidates: OpaqueWalletPrivateRowMutation {
                    updates: &sender_transaction_candidate_updates,
                    deletes: &sender_transaction_candidate_deletes,
                },
            })?;
        if let (Some(metadata), Some(wallet_meta)) = (metadata.as_mut(), wallet_meta.as_ref()) {
            metadata.last_scanned_block = wallet_meta.last_scanned_block;
            metadata.last_scanned_block_hash = wallet_meta.last_scanned_block_hash;
        }
        let (unspent, spent) = wallet_counts.unwrap_or_default();
        tracing::debug!(
            wallet_chain_uuid = %commit.wallet_id(),
            records = utxo_entries.as_ref().map_or(0, Vec::len),
            unspent,
            spent,
            checkpoint_updated = wallet_meta.is_some(),
            encode_elapsed_ms,
            db_elapsed_ms = db_started.elapsed().as_millis(),
            elapsed_ms = started.elapsed().as_millis(),
            "committed encrypted desktop wallet-private state"
        );

        Ok(())
    }

    fn load_wallet_utxos(
        &self,
        wallet_id: &WalletCacheKey,
    ) -> Result<Vec<WalletUtxo>, WalletCacheError> {
        let started = Instant::now();
        let wallet_chain_uuid = self.wallet_chain_uuid()?;
        let private_records = self.db.list_wallet_utxos(wallet_id)?;
        let mut out = Vec::with_capacity(private_records.len());
        for stored in private_records {
            let row_id_bytes =
                alloy::hex::decode(&stored.utxo_id).map_err(|_| WalletCacheError::Crypto)?;
            let row_id: [u8; KEY_LEN] = row_id_bytes
                .try_into()
                .map_err(|_| WalletCacheError::Crypto)?;
            let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
            let plaintext = self
                .cache_keys
                .decrypt_row(&row_id, &record)
                .map_err(|_| WalletCacheError::Crypto)?;
            out.push(deserialize_wallet_utxo(&plaintext)?);
        }
        tracing::debug!(
            wallet_chain_uuid,
            rows = out.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "loaded atomic encrypted desktop wallet cache"
        );
        Ok(out)
    }

    fn get_wallet_meta(
        &self,
        wallet_id: &WalletCacheKey,
    ) -> Result<Option<WalletMeta>, WalletCacheError> {
        Ok(self.db.get_wallet_meta(wallet_id)?)
    }

    fn get_wallet_sync_actor_state(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
    ) -> Result<Option<local_db::WalletSyncActorStateRecord>, WalletCacheError> {
        Ok(self
            .db
            .get_wallet_sync_actor_state(chain_id, wallet_id.as_str())?)
    }

    fn put_wallet_sync_actor_state(
        &self,
        commit: sync_service::types::WalletSyncActorStateCommit<'_>,
    ) -> Result<(), WalletCacheError> {
        Ok(self.db.put_wallet_sync_actor_state(commit.state())?)
    }

    fn get_pending_output_poi_context(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
        output_commitment: &FixedBytes<32>,
    ) -> Result<Option<PendingOutputPoiContextRecord>, WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        let row_id = self.cache_keys.private_row_id(
            WalletPrivateRecordKind::PendingOutputPoiContext,
            namespace.wallet_id.as_str(),
            output_commitment.as_slice(),
        );
        self.db
            .get_opaque_wallet_private_row(
                &namespace,
                WalletPrivateRecordKind::PendingOutputPoiContext,
                &row_id,
            )?
            .map(|row| {
                self.decode_pending_output_row(&namespace, &row)
                    .map_err(|_| WalletCacheError::Crypto)
            })
            .transpose()
    }

    fn list_pending_output_poi_contexts(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
    ) -> Result<Vec<PendingOutputPoiContextRecord>, WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        self.db
            .list_opaque_wallet_private_rows(
                &namespace,
                WalletPrivateRecordKind::PendingOutputPoiContext,
            )?
            .iter()
            .map(|row| {
                self.decode_pending_output_row(&namespace, row)
                    .map_err(|_| WalletCacheError::Crypto)
            })
            .collect()
    }

    fn get_output_poi_recovery(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
        output_commitment: &FixedBytes<32>,
    ) -> Result<Option<OutputPoiRecoveryRecord>, WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        let row_id = self.cache_keys.private_row_id(
            WalletPrivateRecordKind::OutputPoiRecovery,
            namespace.wallet_id.as_str(),
            output_commitment.as_slice(),
        );
        self.db
            .get_opaque_wallet_private_row(
                &namespace,
                WalletPrivateRecordKind::OutputPoiRecovery,
                &row_id,
            )?
            .map(|row| {
                self.decode_output_recovery_row(&namespace, &row)
                    .map_err(|_| WalletCacheError::Crypto)
            })
            .transpose()
    }

    fn list_output_poi_recoveries(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
    ) -> Result<Vec<OutputPoiRecoveryRecord>, WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        self.db
            .list_opaque_wallet_private_rows(
                &namespace,
                WalletPrivateRecordKind::OutputPoiRecovery,
            )?
            .iter()
            .map(|row| {
                self.decode_output_recovery_row(&namespace, row)
                    .map_err(|_| WalletCacheError::Crypto)
            })
            .collect()
    }

    fn get_sender_transaction_candidate(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
        outer_transaction_hash: &FixedBytes<32>,
    ) -> Result<Option<SenderTransactionCandidate>, WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        let row_id = self.cache_keys.private_row_id(
            WalletPrivateRecordKind::SenderTransactionCandidate,
            namespace.wallet_id.as_str(),
            outer_transaction_hash.as_slice(),
        );
        self.db
            .get_opaque_wallet_private_row(
                &namespace,
                WalletPrivateRecordKind::SenderTransactionCandidate,
                &row_id,
            )?
            .map(|row| {
                let candidate = self
                    .decode_sender_transaction_candidate_row(&namespace, &row)
                    .map_err(|_| WalletCacheError::Crypto)?;
                if candidate.semantic_id() != *outer_transaction_hash {
                    return Err(WalletCacheError::Crypto);
                }
                Ok(candidate)
            })
            .transpose()
    }

    fn list_sender_transaction_candidates(
        &self,
        chain_id: u64,
        wallet_id: &WalletCacheKey,
    ) -> Result<Vec<SenderTransactionCandidate>, WalletCacheError> {
        let namespace = self.private_namespace(chain_id, wallet_id)?;
        self.db
            .list_opaque_wallet_private_rows(
                &namespace,
                WalletPrivateRecordKind::SenderTransactionCandidate,
            )?
            .iter()
            .map(|row| {
                self.decode_sender_transaction_candidate_row(&namespace, row)
                    .map_err(|_| WalletCacheError::Crypto)
            })
            .collect()
    }
}
