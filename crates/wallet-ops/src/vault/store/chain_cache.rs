use std::time::{SystemTime, UNIX_EPOCH};

use local_db::{
    OpaqueWalletPrivateRowMutation, WalletMetaMutation, WalletPrivateNamespaceId,
    WalletPrivateStateBatch, WalletSyncActorStateRecord, WalletUtxoRowMutation,
};

use crate::vault::DesktopEncryptedWalletCacheStore;
use crate::{DesktopWalletVaultDbExt as _, DesktopWalletVaultRecord};

use super::{
    DesktopVaultStore, DesktopViewSession, EncryptedRecord, VaultError, WALLET_CHAIN_INDEX_PREFIX,
    WALLET_CHAIN_METADATA_PREFIX, WalletCacheKey, WalletChainMetadataBundle, WalletMeta,
    deserialize_wallet_utxo, generate_opaque_id, serialize_wallet_utxo,
    vault_error_from_wallet_cache, wallet_chain_index_complete_record_entry,
    wallet_chain_index_record_key, wallet_chain_metadata_record_key, wallet_utxo_stable_identity,
};

static WALLET_CHAIN_METADATA_CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalletChainOwnershipResolution {
    Complete,
    UnresolvedUnindexedRecord,
}

struct WalletChainMetadataScan {
    matching_metadata: Option<WalletChainMetadataBundle>,
    ownership_index_records: Vec<(String, Vec<u8>)>,
    ownership_resolution: WalletChainOwnershipResolution,
}

impl DesktopVaultStore {
    pub fn store_wallet_chain_metadata(
        &self,
        password: &str,
        metadata: &WalletChainMetadataBundle,
    ) -> Result<(), VaultError> {
        let view = self.unlock_view(password)?;
        let record = view.encrypt_wallet_chain_metadata(&metadata.wallet_chain_uuid, metadata)?;
        let metadata_key = wallet_chain_metadata_record_key(&metadata.wallet_chain_uuid);
        self.db.put_desktop_wallet_vault_records(&[
            record.to_record_entry(metadata_key)?,
            wallet_chain_index_record_entry(metadata)?,
        ])?;
        Ok(())
    }

    pub fn load_wallet_chain_metadata(
        &self,
        password: &str,
        wallet_chain_uuid: &str,
    ) -> Result<WalletChainMetadataBundle, VaultError> {
        let view = self.unlock_view(password)?;
        let record = self.encrypted_record(&wallet_chain_metadata_record_key(wallet_chain_uuid))?;
        view.decrypt_wallet_chain_metadata(wallet_chain_uuid, &record)
    }

    pub fn wallet_chain_metadata_for_session(
        &self,
        view_session: &DesktopViewSession,
        chain_type: u8,
        chain_id: u64,
        contract: &str,
        start_block: u64,
    ) -> Result<WalletChainMetadataBundle, VaultError> {
        self.find_or_create_wallet_chain_metadata_for_session(
            view_session,
            chain_type,
            chain_id,
            contract,
            start_block,
            start_block.saturating_sub(1),
        )
        .map(|(metadata, _created)| metadata)
    }

    pub fn find_wallet_chain_metadata_for_session(
        &self,
        view_session: &DesktopViewSession,
        chain_type: u8,
        chain_id: u64,
        contract: &str,
    ) -> Result<Option<WalletChainMetadataBundle>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(WALLET_CHAIN_METADATA_PREFIX)?;
        let indexed_metadata_keys = self
            .db
            .list_desktop_wallet_vault_records(WALLET_CHAIN_INDEX_PREFIX)?
            .into_iter()
            .filter_map(|stored| {
                stored.key.rsplit_once('|').map(|(_, wallet_chain_uuid)| {
                    wallet_chain_metadata_record_key(wallet_chain_uuid)
                })
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut scan = WalletChainMetadataScan {
            matching_metadata: None,
            ownership_index_records: Vec::new(),
            ownership_resolution: WalletChainOwnershipResolution::Complete,
        };
        for stored in records {
            let Some(wallet_chain_uuid) = stored.key.strip_prefix(WALLET_CHAIN_METADATA_PREFIX)
            else {
                continue;
            };
            let Ok(record) = rmp_serde::from_slice::<EncryptedRecord>(&stored.payload) else {
                tracing::warn!("ignoring invalid wallet chain metadata record during lookup");
                if !indexed_metadata_keys.contains(&stored.key) {
                    scan.ownership_resolution =
                        WalletChainOwnershipResolution::UnresolvedUnindexedRecord;
                }
                continue;
            };
            let metadata =
                match view_session.decrypt_wallet_chain_metadata(wallet_chain_uuid, &record) {
                    Ok(metadata) => metadata,
                    Err(VaultError::Decrypt) => continue,
                    Err(_) => {
                        if !indexed_metadata_keys.contains(&stored.key) {
                            scan.ownership_resolution =
                                WalletChainOwnershipResolution::UnresolvedUnindexedRecord;
                        }
                        continue;
                    }
                };
            let (index_key, index_payload) = wallet_chain_index_record_entry(&metadata)?;
            scan.ownership_index_records
                .push((index_key, index_payload));
            if metadata.wallet_uuid == view_session.wallet_id()
                && metadata.chain_type == chain_type
                && metadata.chain_id == chain_id
                && metadata.contract.eq_ignore_ascii_case(contract)
                && scan.matching_metadata.is_none()
            {
                scan.matching_metadata = Some(metadata);
            }
        }

        if scan.ownership_resolution == WalletChainOwnershipResolution::Complete {
            scan.ownership_index_records
                .push(wallet_chain_index_complete_record_entry(
                    view_session.wallet_id(),
                )?);
        }
        self.db
            .put_desktop_wallet_vault_records(&scan.ownership_index_records)?;
        Ok(scan.matching_metadata)
    }

    pub fn create_wallet_chain_metadata_for_session(
        &self,
        view_session: &DesktopViewSession,
        chain_type: u8,
        chain_id: u64,
        contract: &str,
        start_block: u64,
        last_scanned_block: u64,
    ) -> Result<WalletChainMetadataBundle, VaultError> {
        let wallet_chain_uuid = generate_opaque_id()?;
        let metadata = WalletChainMetadataBundle {
            wallet_chain_uuid,
            wallet_uuid: view_session.wallet_id().to_owned(),
            chain_type,
            chain_id,
            contract: contract.to_owned(),
            start_block,
            last_scanned_block,
            last_scanned_block_hash: None,
            poi_read_source: None,
        };
        self.store_wallet_chain_metadata_with_session(view_session, &metadata)?;
        Ok(metadata)
    }

    pub fn find_or_create_wallet_chain_metadata_for_session(
        &self,
        view_session: &DesktopViewSession,
        chain_type: u8,
        chain_id: u64,
        contract: &str,
        start_block: u64,
        last_scanned_block: u64,
    ) -> Result<(WalletChainMetadataBundle, bool), VaultError> {
        let _guard = WALLET_CHAIN_METADATA_CREATE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(metadata) = self.find_wallet_chain_metadata_for_session(
            view_session,
            chain_type,
            chain_id,
            contract,
        )? {
            return Ok((metadata, false));
        }

        self.create_wallet_chain_metadata_for_session(
            view_session,
            chain_type,
            chain_id,
            contract,
            start_block,
            last_scanned_block,
        )?;
        let metadata = self
            .find_wallet_chain_metadata_for_session(view_session, chain_type, chain_id, contract)?
            .ok_or(VaultError::WalletChainMetadataUnavailable)?;
        Ok((metadata, true))
    }

    pub fn store_wallet_chain_metadata_with_session(
        &self,
        view_session: &DesktopViewSession,
        metadata: &WalletChainMetadataBundle,
    ) -> Result<(), VaultError> {
        let record =
            view_session.encrypt_wallet_chain_metadata(&metadata.wallet_chain_uuid, metadata)?;
        let metadata_key = wallet_chain_metadata_record_key(&metadata.wallet_chain_uuid);
        self.db.put_desktop_wallet_vault_records(&[
            record.to_record_entry(metadata_key)?,
            wallet_chain_index_record_entry(metadata)?,
        ])?;
        Ok(())
    }

    pub fn reset_wallet_chain_cache_with_session(
        &self,
        view_session: &DesktopViewSession,
        metadata: &mut WalletChainMetadataBundle,
        start_block: u64,
    ) -> Result<(), VaultError> {
        let candidate_deletes =
            self.sender_transaction_candidate_rewind_row_ids(view_session, metadata, start_block)?;
        self.commit_wallet_chain_cache_repair(
            view_session,
            metadata,
            &[],
            &candidate_deletes,
            start_block,
        )
    }

    pub fn rewind_wallet_chain_cache_with_session(
        &self,
        view_session: &DesktopViewSession,
        metadata: &mut WalletChainMetadataBundle,
        start_block: u64,
    ) -> Result<(), VaultError> {
        let candidate_deletes =
            self.sender_transaction_candidate_rewind_row_ids(view_session, metadata, start_block)?;
        let cache_keys = view_session.derive_cache_keys(&metadata.wallet_chain_uuid)?;
        let wallet_id = metadata.wallet_chain_uuid.parse::<WalletCacheKey>()?;
        let existing_rows = self.db.list_wallet_utxos(&wallet_id)?;
        let mut records = Vec::with_capacity(existing_rows.len());
        let mut dropped_rows = 0usize;
        let mut cleared_spent_rows = 0usize;
        let mut invalid_rows = 0usize;

        for stored in existing_rows {
            let Ok(row_id_bytes) = alloy::hex::decode(&stored.utxo_id) else {
                invalid_rows += 1;
                continue;
            };
            let Ok(row_id) = row_id_bytes.try_into() else {
                invalid_rows += 1;
                continue;
            };
            let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
            let plaintext = cache_keys
                .decrypt_row(&row_id, &record)
                .map_err(|_| VaultError::Decrypt)?;
            let mut utxo =
                deserialize_wallet_utxo(&plaintext).map_err(vault_error_from_wallet_cache)?;
            if utxo.utxo.source.block_number >= start_block {
                dropped_rows += 1;
                continue;
            }
            if utxo
                .spent
                .as_ref()
                .is_some_and(|spent| spent.block_number >= start_block)
            {
                utxo.spent = None;
                cleared_spent_rows += 1;
            }

            let stable_identity = wallet_utxo_stable_identity(&utxo);
            let expected_row_id =
                cache_keys.row_id(utxo.utxo.tree, utxo.utxo.position, &stable_identity);
            if expected_row_id != row_id {
                invalid_rows += 1;
                continue;
            }

            let plaintext = serialize_wallet_utxo(&utxo).map_err(vault_error_from_wallet_cache)?;
            let record = cache_keys
                .encrypt_row(&row_id, &plaintext)
                .map_err(|_| VaultError::Encrypt)?;
            let data = rmp_serde::to_vec_named(&record)?;
            records.push((alloy::hex::encode(row_id), data));
        }

        self.commit_wallet_chain_cache_repair(
            view_session,
            metadata,
            &records,
            &candidate_deletes,
            start_block,
        )?;
        tracing::info!(
            wallet_chain_uuid = %metadata.wallet_chain_uuid,
            start_block,
            retained_rows = records.len(),
            dropped_rows,
            cleared_spent_rows,
            invalid_rows,
            "rewound encrypted desktop wallet cache"
        );
        Ok(())
    }

    fn sender_transaction_candidate_rewind_row_ids(
        &self,
        view_session: &DesktopViewSession,
        metadata: &WalletChainMetadataBundle,
        start_block: u64,
    ) -> Result<Vec<Vec<u8>>, VaultError> {
        let wallet_id = metadata.wallet_chain_uuid.parse::<WalletCacheKey>()?;
        let cache = DesktopEncryptedWalletCacheStore::for_chain_cache_repair(
            self.db.clone(),
            view_session,
            metadata.clone(),
        )?;
        cache.sender_transaction_candidate_rewind_row_ids(
            metadata.chain_id,
            &wallet_id,
            start_block,
        )
    }

    fn commit_wallet_chain_cache_repair(
        &self,
        view_session: &DesktopViewSession,
        metadata: &mut WalletChainMetadataBundle,
        records: &[(String, Vec<u8>)],
        candidate_deletes: &[Vec<u8>],
        start_block: u64,
    ) -> Result<(), VaultError> {
        let last_scanned_block = start_block.saturating_sub(1);
        let mut repaired_metadata = metadata.clone();
        repaired_metadata.start_block = repaired_metadata.start_block.min(start_block);
        repaired_metadata.last_scanned_block = last_scanned_block;
        repaired_metadata.last_scanned_block_hash = None;
        let encrypted_metadata = view_session.encrypt_wallet_chain_metadata(
            &repaired_metadata.wallet_chain_uuid,
            &repaired_metadata,
        )?;
        let (metadata_key, metadata_payload) = encrypted_metadata.to_record_entry(
            wallet_chain_metadata_record_key(&repaired_metadata.wallet_chain_uuid),
        )?;
        let (index_key, index_payload) = wallet_chain_index_record_entry(&repaired_metadata)?;
        let vault_records = [
            DesktopWalletVaultRecord {
                key: metadata_key,
                payload: metadata_payload,
            },
            DesktopWalletVaultRecord {
                key: index_key,
                payload: index_payload,
            },
        ];
        let wallet_id = metadata.wallet_chain_uuid.parse::<WalletCacheKey>()?;
        let actor_wallet_id = wallet_id.to_string();
        let highest_accepted_reset_intent = self
            .db
            .get_wallet_sync_actor_state(metadata.chain_id, &actor_wallet_id)?
            .map_or(0, |state| state.highest_accepted_reset_intent);
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        let wallet_meta = WalletMeta {
            last_scanned_block,
            updated_at: 0,
            last_scanned_block_hash: None,
        };
        let sync_actor_state = WalletSyncActorStateRecord {
            chain_id: metadata.chain_id,
            wallet_id: actor_wallet_id,
            highest_accepted_reset_intent,
            pending_reset: None,
            updated_at,
        };
        let namespace = WalletPrivateNamespaceId::new(metadata.chain_id, wallet_id);
        self.db
            .batch_commit_wallet_private_state_with_vault_records(
                &WalletPrivateStateBatch {
                    namespace: &namespace,
                    utxos: WalletUtxoRowMutation::Replace(records),
                    metadata: WalletMetaMutation::Set(&wallet_meta),
                    sync_actor_state: Some(&sync_actor_state),
                    pending_output_contexts: OpaqueWalletPrivateRowMutation::default(),
                    output_poi_recoveries: OpaqueWalletPrivateRowMutation::default(),
                    sender_transaction_candidates: OpaqueWalletPrivateRowMutation {
                        updates: &[],
                        deletes: candidate_deletes,
                    },
                },
                &vault_records,
            )?;

        *metadata = repaired_metadata;
        Ok(())
    }
}

fn wallet_chain_index_record_entry(
    metadata: &WalletChainMetadataBundle,
) -> Result<(String, Vec<u8>), VaultError> {
    Ok((
        wallet_chain_index_record_key(&metadata.wallet_uuid, &metadata.wallet_chain_uuid),
        rmp_serde::to_vec_named(&metadata.chain_id)?,
    ))
}
