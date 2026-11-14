//! Public Tron accounts, under their own record prefixes so that no Public EVM account listing
//! returns them.

use alloy::primitives::Address;

use super::{
    DesktopVaultStore, DesktopViewSession, EncryptedRecord, HardwareDerivationDescriptor, KEY_LEN,
    ProtectedSoftwareSeedSession, PublicAccountScope, PublicAccountSecret, PublicAccountStatus,
    SpendGrant, TRON_ACCOUNT_METADATA_PREFIX, TronAccountMetadata, TronAccountSource, VaultError,
    ViewUnlock, Zeroizing, derive_tron_private_key_from_seed,
    ensure_tron_account_address_available, generate_opaque_id, next_derived_tron_account_index,
    next_tron_account_display_order, normalize_public_account_label, parse_public_evm_private_key,
    public_evm_address_from_private_key, sort_tron_account_metadata,
    tron_account_metadata_record_entry, tron_account_metadata_record_key,
    tron_account_secret_record_entry, tron_account_secret_record_key, unlock_spend, unlock_view,
};
use crate::DesktopWalletVaultDbExt as _;

impl DesktopVaultStore {
    pub fn list_tron_accounts_for_session(
        &self,
        view_session: &DesktopViewSession,
        include_inactive: bool,
    ) -> Result<Vec<TronAccountMetadata>, VaultError> {
        let mut accounts = self.list_tron_account_metadata_with_view(&view_session.view)?;
        let wallet_id = view_session.wallet_id();
        accounts.retain(|account| {
            account.scope.includes_wallet(wallet_id)
                && (include_inactive || account.status == PublicAccountStatus::Active)
        });
        Ok(accounts)
    }

    /// Adds the software wallet's next derived Tron account. Indexes are counted apart from
    /// Public accounts, and a hidden account keeps its index.
    pub fn add_derived_tron_account(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        label: Option<&str>,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<TronAccountMetadata, VaultError> {
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let mut grant = SpendGrant::one_use(unlock_spend(&vault_metadata, password)?);
        let wallet_id = view_session.wallet_id();
        let accounts = self.list_tron_account_metadata_with_view(&view)?;
        let index = next_derived_tron_account_index(&accounts, wallet_id)?;
        let address = public_evm_address_from_private_key(&*self.derived_tron_private_key(
            &view,
            &mut grant,
            wallet_id,
            index,
            protected_seed_session,
        )?)?;
        self.put_derived_tron_account(
            &view,
            &accounts,
            wallet_id,
            address,
            label,
            TronAccountSource::Derived { index },
        )
    }

    /// Adds a hardware-derived wallet's next Tron account. `entropy` is the device's fresh
    /// derivation output for `descriptor`; the synthetic seed it rebuilds is dropped on return.
    pub fn add_hardware_derived_tron_account(
        &self,
        view_session: &DesktopViewSession,
        descriptor: &HardwareDerivationDescriptor,
        entropy: &[u8],
        label: Option<&str>,
    ) -> Result<TronAccountMetadata, VaultError> {
        let wallet_id = view_session.wallet_id();
        let accounts = self.list_tron_account_metadata_with_view(&view_session.view)?;
        let index = next_derived_tron_account_index(&accounts, wallet_id)?;
        let address = public_evm_address_from_private_key(
            &*self.hardware_derived_tron_private_key(view_session, descriptor, entropy, index)?,
        )?;
        self.put_derived_tron_account(
            &view_session.view,
            &accounts,
            wallet_id,
            address,
            label,
            TronAccountSource::HardwareDerived { index },
        )
    }

    fn put_derived_tron_account(
        &self,
        view: &ViewUnlock,
        accounts: &[TronAccountMetadata],
        wallet_id: &str,
        address: Address,
        label: Option<&str>,
        source: TronAccountSource,
    ) -> Result<TronAccountMetadata, VaultError> {
        let scope = PublicAccountScope::PrivateWallet {
            wallet_uuid: wallet_id.to_owned(),
        };
        ensure_tron_account_address_available(accounts, address, &scope, wallet_id)?;

        let account = TronAccountMetadata {
            tron_account_uuid: generate_opaque_id()?,
            address,
            label: normalize_public_account_label(label),
            source,
            scope,
            status: PublicAccountStatus::Active,
            display_order: next_tron_account_display_order(accounts)?,
        };
        let (key, data) = tron_account_metadata_record_entry(view, &account)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(account)
    }

    pub fn import_tron_account(
        &self,
        password: &str,
        view_session: &DesktopViewSession,
        private_key_hex: &str,
        label: Option<&str>,
        global: bool,
    ) -> Result<TronAccountMetadata, VaultError> {
        let vault_metadata = self.metadata()?;
        let view = unlock_view(&vault_metadata, password)?;
        let spend = unlock_spend(&vault_metadata, password)?;
        let private_key = parse_public_evm_private_key(private_key_hex)?;
        let address = public_evm_address_from_private_key(&private_key)?;
        let accounts = self.list_tron_account_metadata_with_view(&view)?;
        let scope = if global {
            PublicAccountScope::Global
        } else {
            PublicAccountScope::PrivateWallet {
                wallet_uuid: view_session.wallet_id().to_owned(),
            }
        };
        ensure_tron_account_address_available(
            &accounts,
            address,
            &scope,
            view_session.wallet_id(),
        )?;

        let account = TronAccountMetadata {
            tron_account_uuid: generate_opaque_id()?,
            address,
            label: normalize_public_account_label(label),
            source: TronAccountSource::Imported,
            scope,
            status: PublicAccountStatus::Active,
            display_order: next_tron_account_display_order(&accounts)?,
        };
        let secret = PublicAccountSecret {
            private_key: *private_key,
        };
        let metadata_entry = tron_account_metadata_record_entry(&view, &account)?;
        let secret_entry = tron_account_secret_record_entry(&spend, &account, &secret)?;
        self.db
            .put_desktop_wallet_vault_records(&[metadata_entry, secret_entry])?;
        Ok(account)
    }

    pub fn update_tron_account_label(
        &self,
        view_session: &DesktopViewSession,
        tron_account_uuid: &str,
        label: Option<&str>,
    ) -> Result<TronAccountMetadata, VaultError> {
        let mut account = self
            .list_tron_accounts_for_session(view_session, true)?
            .into_iter()
            .find(|account| account.tron_account_uuid == tron_account_uuid)
            .ok_or(VaultError::PublicAccountNotFound)?;
        account.label = normalize_public_account_label(label);
        let (key, data) = tron_account_metadata_record_entry(&view_session.view, &account)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(account)
    }

    /// Hides or shows a derived Tron account. Showing it is refused when its address is
    /// already an active account in the same list.
    pub fn set_derived_tron_account_active(
        &self,
        view_session: &DesktopViewSession,
        tron_account_uuid: &str,
        active: bool,
    ) -> Result<TronAccountMetadata, VaultError> {
        let accounts = self.list_tron_accounts_for_session(view_session, true)?;
        let Some(account) = accounts
            .iter()
            .find(|account| account.tron_account_uuid == tron_account_uuid)
        else {
            return Err(VaultError::PublicAccountNotFound);
        };
        if account.source.derivation_index().is_none() {
            return Err(VaultError::InvalidPublicAccountOperation);
        }
        let status = if active {
            PublicAccountStatus::Active
        } else {
            PublicAccountStatus::Inactive
        };
        if account.status == status {
            return Ok(account.clone());
        }
        if active {
            ensure_tron_account_address_available(
                &accounts,
                account.address,
                &account.scope,
                view_session.wallet_id(),
            )?;
        }
        let updated = TronAccountMetadata {
            status,
            ..account.clone()
        };
        let (key, data) = tron_account_metadata_record_entry(&view_session.view, &updated)?;
        self.db.put_desktop_wallet_vault_record(&key, &data)?;
        Ok(updated)
    }

    pub fn delete_imported_tron_account(
        &self,
        view_session: &DesktopViewSession,
        tron_account_uuid: &str,
    ) -> Result<TronAccountMetadata, VaultError> {
        let _activity_guard = crate::vault::activity::ACTIVITY_RECORD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let account = self
            .list_tron_accounts_for_session(view_session, false)?
            .into_iter()
            .find(|account| account.tron_account_uuid == tron_account_uuid)
            .ok_or(VaultError::PublicAccountNotFound)?;
        if account.source != TronAccountSource::Imported {
            return Err(VaultError::InvalidPublicAccountOperation);
        }

        let mut keys = self.activity_account_keys(&account.tron_account_uuid)?;
        keys.push(tron_account_metadata_record_key(&account.tron_account_uuid));
        keys.push(tron_account_secret_record_key(&account.tron_account_uuid));
        self.db.update_desktop_wallet_vault_records(&keys, &[])?;
        Ok(account)
    }

    /// The account's signing key, for derived software and imported accounts.
    pub fn tron_account_signing_key(
        &self,
        grant: &mut SpendGrant,
        view_session: &DesktopViewSession,
        tron_account_uuid: &str,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
        let account = self
            .list_tron_accounts_for_session(view_session, true)?
            .into_iter()
            .find(|account| account.tron_account_uuid == tron_account_uuid)
            .ok_or(VaultError::PublicAccountNotFound)?;
        match account.source {
            TronAccountSource::Derived { index } => {
                let private_key = self.derived_tron_private_key(
                    &view_session.view,
                    grant,
                    view_session.wallet_id(),
                    index,
                    protected_seed_session,
                )?;
                if public_evm_address_from_private_key(&private_key)? != account.address {
                    return Err(VaultError::InvalidSoftwareContextIdentity);
                }
                Ok(private_key)
            }
            TronAccountSource::Imported => {
                let spend = grant.take_spend_unlock()?;
                let record = self.encrypted_record(&tron_account_secret_record_key(
                    &account.tron_account_uuid,
                ))?;
                let secret =
                    spend.decrypt_tron_account_secret(&account.tron_account_uuid, &record)?;
                Ok(Zeroizing::new(secret.private_key))
            }
            // Needs fresh device output: `hardware_derived_tron_account_signing_key`.
            TronAccountSource::HardwareDerived { .. } => {
                Err(VaultError::InvalidPublicAccountOperation)
            }
        }
    }

    /// A hardware-derived Tron account's signing key, from the device's fresh derivation output
    /// for `descriptor`. Nothing is cached, so every signature needs the device again.
    pub fn hardware_derived_tron_account_signing_key(
        &self,
        view_session: &DesktopViewSession,
        tron_account_uuid: &str,
        descriptor: &HardwareDerivationDescriptor,
        entropy: &[u8],
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
        let account = self
            .list_tron_accounts_for_session(view_session, true)?
            .into_iter()
            .find(|account| account.tron_account_uuid == tron_account_uuid)
            .ok_or(VaultError::PublicAccountNotFound)?;
        let TronAccountSource::HardwareDerived { index } = account.source else {
            return Err(VaultError::InvalidPublicAccountOperation);
        };
        let private_key =
            self.hardware_derived_tron_private_key(view_session, descriptor, entropy, index)?;
        if public_evm_address_from_private_key(&private_key)? != account.address {
            return Err(VaultError::HardwareWalletIdentityMismatch);
        }
        Ok(private_key)
    }

    /// The Tron key at `index` of the synthetic seed that `entropy` rebuilds, after the same
    /// custody and identity checks executor authorization uses.
    fn hardware_derived_tron_private_key(
        &self,
        view_session: &DesktopViewSession,
        descriptor: &HardwareDerivationDescriptor,
        entropy: &[u8],
        index: u32,
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
        let (seed, _signer) =
            self.hardware_seed_and_signer_for_session(view_session, descriptor, entropy)?;
        derive_tron_private_key_from_seed(&seed, index)
    }

    /// The software wallet's Tron key at `index`, authorized as a derived Public account key is.
    fn derived_tron_private_key(
        &self,
        view: &ViewUnlock,
        grant: &mut SpendGrant,
        wallet_id: &str,
        index: u32,
        protected_seed_session: Option<&ProtectedSoftwareSeedSession>,
    ) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
        let seed = self.software_wallet_seed(view, grant, wallet_id, protected_seed_session)?;
        derive_tron_private_key_from_seed(&seed, index)
    }

    pub(in crate::vault) fn list_tron_account_metadata_with_view(
        &self,
        view: &ViewUnlock,
    ) -> Result<Vec<TronAccountMetadata>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(TRON_ACCOUNT_METADATA_PREFIX)?;
        let mut accounts = Vec::with_capacity(records.len());
        for stored in records {
            let Some(tron_account_uuid) = stored.key.strip_prefix(TRON_ACCOUNT_METADATA_PREFIX)
            else {
                continue;
            };
            let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
            let mut account = view.decrypt_tron_account_metadata(tron_account_uuid, &record)?;
            if account.tron_account_uuid != tron_account_uuid {
                tron_account_uuid.clone_into(&mut account.tron_account_uuid);
            }
            accounts.push(account);
        }
        sort_tron_account_metadata(&mut accounts);
        Ok(accounts)
    }
}
