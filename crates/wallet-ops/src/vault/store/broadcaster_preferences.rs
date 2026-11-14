use super::{
    BROADCASTER_BANNED_PREFIX, BROADCASTER_FAVORITE_PREFIX, BTreeSet, BroadcasterAddressIdentity,
    BroadcasterPreferenceEntry, BroadcasterPreferences, DesktopVaultStore, DesktopViewSession,
    EncryptedRecord, RecordKind, VaultError, ViewUnlock, broadcaster_banned_record_entry,
    broadcaster_banned_record_key, broadcaster_favorite_record_entry,
    broadcaster_favorite_record_key, broadcaster_preference_entry_identity, generate_opaque_id,
    sort_broadcaster_preference_entries, validate_broadcaster_preference_address,
};
use crate::DesktopWalletVaultDbExt as _;

#[derive(Clone)]
struct LoadedBroadcasterPreferenceEntry {
    entry_uuid: String,
    entry: BroadcasterPreferenceEntry,
    identity: BroadcasterAddressIdentity,
}

impl DesktopVaultStore {
    pub fn list_broadcaster_preferences_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<BroadcasterPreferences, VaultError> {
        let banned = self.list_broadcaster_banned_entries_with_view(&view_session.view)?;
        let banned_identities = banned
            .iter()
            .map(|loaded| loaded.identity)
            .collect::<BTreeSet<_>>();
        let mut favorites = self
            .list_broadcaster_favorite_entries_with_view(&view_session.view)?
            .into_iter()
            .filter(|loaded| !banned_identities.contains(&loaded.identity))
            .map(|loaded| loaded.entry)
            .collect::<Vec<_>>();
        let mut banned = banned
            .into_iter()
            .map(|loaded| loaded.entry)
            .collect::<Vec<_>>();
        sort_broadcaster_preference_entries(&mut favorites);
        sort_broadcaster_preference_entries(&mut banned);
        Ok(BroadcasterPreferences { favorites, banned })
    }

    pub fn list_favorite_broadcasters_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<BroadcasterPreferenceEntry>, VaultError> {
        Ok(self
            .list_broadcaster_preferences_for_session(view_session)?
            .favorites)
    }

    pub fn list_banned_broadcasters_for_session(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<BroadcasterPreferenceEntry>, VaultError> {
        Ok(self
            .list_broadcaster_preferences_for_session(view_session)?
            .banned)
    }

    pub fn add_favorite_broadcaster_for_session(
        &self,
        view_session: &DesktopViewSession,
        address: &str,
    ) -> Result<BroadcasterPreferenceEntry, VaultError> {
        let (address, identity) = validate_broadcaster_preference_address(address)?;
        let favorites = self.list_broadcaster_favorite_entries_with_view(&view_session.view)?;
        let banned = self.list_broadcaster_banned_entries_with_view(&view_session.view)?;
        let existing = favorites
            .iter()
            .find(|loaded| loaded.identity == identity)
            .cloned();
        for loaded in banned.iter().filter(|loaded| loaded.identity == identity) {
            self.db
                .delete_desktop_wallet_vault_record(&broadcaster_banned_record_key(
                    &loaded.entry_uuid,
                ))?;
        }
        if let Some(existing) = existing {
            return Ok(existing.entry);
        }

        let entry_uuid = generate_opaque_id()?;
        let entry = BroadcasterPreferenceEntry { address };
        let (key, data) =
            broadcaster_favorite_record_entry(&view_session.view, &entry_uuid, &entry)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(entry)
    }

    pub fn add_banned_broadcaster_for_session(
        &self,
        view_session: &DesktopViewSession,
        address: &str,
    ) -> Result<BroadcasterPreferenceEntry, VaultError> {
        let (address, identity) = validate_broadcaster_preference_address(address)?;
        let favorites = self.list_broadcaster_favorite_entries_with_view(&view_session.view)?;
        let banned = self.list_broadcaster_banned_entries_with_view(&view_session.view)?;
        let existing = banned
            .iter()
            .find(|loaded| loaded.identity == identity)
            .cloned();
        for loaded in favorites
            .iter()
            .filter(|loaded| loaded.identity == identity)
        {
            self.db
                .delete_desktop_wallet_vault_record(&broadcaster_favorite_record_key(
                    &loaded.entry_uuid,
                ))?;
        }
        if let Some(existing) = existing {
            return Ok(existing.entry);
        }

        let entry_uuid = generate_opaque_id()?;
        let entry = BroadcasterPreferenceEntry { address };
        let (key, data) = broadcaster_banned_record_entry(&view_session.view, &entry_uuid, &entry)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(entry)
    }

    pub fn remove_favorite_broadcaster_for_session(
        &self,
        view_session: &DesktopViewSession,
        address: &str,
    ) -> Result<Option<BroadcasterPreferenceEntry>, VaultError> {
        let (_, identity) = validate_broadcaster_preference_address(address)?;
        let favorites = self.list_broadcaster_favorite_entries_with_view(&view_session.view)?;
        let Some(loaded) = favorites
            .into_iter()
            .find(|loaded| loaded.identity == identity)
        else {
            return Ok(None);
        };
        self.db
            .delete_desktop_wallet_vault_record(&broadcaster_favorite_record_key(
                &loaded.entry_uuid,
            ))?;
        Ok(Some(loaded.entry))
    }

    pub fn remove_banned_broadcaster_for_session(
        &self,
        view_session: &DesktopViewSession,
        address: &str,
    ) -> Result<Option<BroadcasterPreferenceEntry>, VaultError> {
        let (_, identity) = validate_broadcaster_preference_address(address)?;
        let banned = self.list_broadcaster_banned_entries_with_view(&view_session.view)?;
        let Some(loaded) = banned
            .into_iter()
            .find(|loaded| loaded.identity == identity)
        else {
            return Ok(None);
        };
        self.db
            .delete_desktop_wallet_vault_record(&broadcaster_banned_record_key(
                &loaded.entry_uuid,
            ))?;
        Ok(Some(loaded.entry))
    }

    fn list_broadcaster_favorite_entries_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<LoadedBroadcasterPreferenceEntry>, VaultError> {
        self.list_broadcaster_preference_entries_with_view(
            view,
            BROADCASTER_FAVORITE_PREFIX,
            RecordKind::BroadcasterFavoriteEntry,
            "favorite",
        )
    }

    fn list_broadcaster_banned_entries_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<LoadedBroadcasterPreferenceEntry>, VaultError> {
        self.list_broadcaster_preference_entries_with_view(
            view,
            BROADCASTER_BANNED_PREFIX,
            RecordKind::BroadcasterBannedEntry,
            "banned",
        )
    }

    fn list_broadcaster_preference_entries_with_view(
        &self,
        view: &ViewUnlock,
        prefix: &str,
        kind: RecordKind,
        preference_kind: &str,
    ) -> Result<Vec<LoadedBroadcasterPreferenceEntry>, VaultError> {
        let records = self.db.list_desktop_wallet_vault_records(prefix)?;
        let mut entries = Vec::with_capacity(records.len());
        let mut seen = BTreeSet::new();
        for stored in records {
            let Some(entry_uuid) = stored.key.strip_prefix(prefix) else {
                continue;
            };
            let Ok(record) = rmp_serde::from_slice::<EncryptedRecord>(&stored.payload) else {
                tracing::warn!(
                    kind = preference_kind,
                    "ignoring invalid broadcaster preference record"
                );
                continue;
            };
            let Ok(entry) = view.decrypt_broadcaster_preference_entry(kind, entry_uuid, &record)
            else {
                tracing::warn!(
                    kind = preference_kind,
                    "ignoring undecryptable broadcaster preference record"
                );
                continue;
            };
            let Ok(identity) = broadcaster_preference_entry_identity(&entry) else {
                tracing::warn!(
                    kind = preference_kind,
                    "ignoring invalid broadcaster preference address"
                );
                continue;
            };
            if seen.insert(identity) {
                entries.push(LoadedBroadcasterPreferenceEntry {
                    entry_uuid: entry_uuid.to_owned(),
                    entry,
                    identity,
                });
            }
        }
        entries.sort_by(|left, right| left.entry.address.cmp(&right.entry.address));
        Ok(entries)
    }
}
