use super::{
    DesktopVaultStore, DesktopViewSession, EncryptedRecord, KEY_LEN, PublicAccountScope,
    PublicAccountStatus, VaultError, WALLETCONNECT_RELAY_IDENTITY_PREFIX,
    WALLETCONNECT_SESSION_PREFIX, WalletConnectRelayIdentity,
    WalletConnectSessionAccountResolution, WalletConnectSessionLifecycleState,
    WalletConnectSessionRecord, fill, public_account_metadata_record_key,
    sort_walletconnect_sessions, walletconnect_relay_identity_record_entry,
    walletconnect_relay_identity_record_key, walletconnect_session_record_entry,
    walletconnect_session_record_key,
};

impl DesktopVaultStore {
    pub fn load_walletconnect_relay_identity(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Option<WalletConnectRelayIdentity>, VaultError> {
        self.load_walletconnect_relay_identity_for_wallet(view_session, view_session.wallet_id())
    }

    pub fn load_walletconnect_relay_identity_for_wallet(
        &self,
        view_session: &DesktopViewSession,
        wallet_uuid: &str,
    ) -> Result<Option<WalletConnectRelayIdentity>, VaultError> {
        let key = walletconnect_relay_identity_record_key(wallet_uuid);
        let Some(record) = self.encrypted_record_optional(&key)? else {
            return Ok(None);
        };
        let mut identity = view_session
            .view
            .decrypt_walletconnect_relay_identity(wallet_uuid, &record)?;
        let auth = crate::WalletConnectRelayClientAuth::from_signing_key(identity.signing_key);
        if identity.client_id != auth.client_id {
            identity.client_id.clone_from(&auth.client_id);
            let (key, payload) = walletconnect_relay_identity_record_entry(
                &view_session.view,
                wallet_uuid,
                &identity,
            )?;
            self.db.put_desktop_wallet_vault_record(&key, &payload)?;
        }
        Ok(Some(identity))
    }

    pub fn load_walletconnect_relay_identity_for_client_id(
        &self,
        view_session: &DesktopViewSession,
        client_id: &str,
    ) -> Result<Option<WalletConnectRelayIdentity>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(WALLETCONNECT_RELAY_IDENTITY_PREFIX)?;
        for stored in records {
            let Some(wallet_uuid) = stored.key.strip_prefix(WALLETCONNECT_RELAY_IDENTITY_PREFIX)
            else {
                continue;
            };
            let Some(identity) =
                self.load_walletconnect_relay_identity_for_wallet(view_session, wallet_uuid)?
            else {
                continue;
            };
            if identity.client_id == client_id {
                return Ok(Some(identity));
            }
        }
        Ok(None)
    }

    pub fn load_or_create_walletconnect_relay_identity(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<WalletConnectRelayIdentity, VaultError> {
        if let Some(identity) = self.load_walletconnect_relay_identity(view_session)? {
            return Ok(identity);
        }

        let mut signing_key = [0u8; KEY_LEN];
        fill(&mut signing_key).map_err(|_| VaultError::Random)?;
        let auth = crate::WalletConnectRelayClientAuth::from_signing_key(signing_key);
        let identity = WalletConnectRelayIdentity {
            signing_key,
            client_id: auth.client_id.clone(),
        };
        let (key, payload) = walletconnect_relay_identity_record_entry(
            &view_session.view,
            view_session.wallet_id(),
            &identity,
        )?;
        self.db.put_desktop_wallet_vault_record(&key, &payload)?;
        Ok(identity)
    }

    pub fn store_walletconnect_session(
        &self,
        view_session: &DesktopViewSession,
        session: &WalletConnectSessionRecord,
    ) -> Result<(), VaultError> {
        let (key, payload) = walletconnect_session_record_entry(&view_session.view, session)?;
        self.db.put_desktop_wallet_vault_record(&key, &payload)?;
        Ok(())
    }

    pub fn list_walletconnect_sessions(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<WalletConnectSessionRecord>, VaultError> {
        let records = self
            .db
            .list_desktop_wallet_vault_records(WALLETCONNECT_SESSION_PREFIX)?;
        let mut sessions = Vec::with_capacity(records.len());
        for stored in records {
            let Some(session_uuid) = stored.key.strip_prefix(WALLETCONNECT_SESSION_PREFIX) else {
                continue;
            };
            let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
            let mut session = view_session
                .view
                .decrypt_walletconnect_session(session_uuid, &record)?;
            if session.session_uuid != session_uuid {
                session_uuid.clone_into(&mut session.session_uuid);
            }
            sessions.push(session);
        }
        sort_walletconnect_sessions(&mut sessions);
        Ok(sessions)
    }

    pub fn load_walletconnect_session(
        &self,
        view_session: &DesktopViewSession,
        session_uuid: &str,
    ) -> Result<WalletConnectSessionRecord, VaultError> {
        let key = walletconnect_session_record_key(session_uuid);
        let record = self.encrypted_record(&key)?;
        view_session
            .view
            .decrypt_walletconnect_session(session_uuid, &record)
    }

    pub fn update_walletconnect_session(
        &self,
        view_session: &DesktopViewSession,
        session: &WalletConnectSessionRecord,
    ) -> Result<(), VaultError> {
        self.store_walletconnect_session(view_session, session)
    }

    pub fn delete_walletconnect_session(&self, session_uuid: &str) -> Result<(), VaultError> {
        self.db
            .delete_desktop_wallet_vault_record(&walletconnect_session_record_key(session_uuid))?;
        Ok(())
    }

    pub fn resolve_walletconnect_session_account(
        &self,
        view_session: &DesktopViewSession,
        session: &WalletConnectSessionRecord,
    ) -> Result<WalletConnectSessionAccountResolution, VaultError> {
        self.resolve_dapp_session_account(
            view_session,
            &session.selected_public_account_uuid,
            &session.selected_public_account_scope,
            session.owning_private_wallet_uuid.as_deref(),
        )
    }

    /// Resolve public account metadata using a matching vault view capability.
    ///
    /// Callers must supply a `DesktopViewSession` for this vault and check that
    /// its unlock and active wallet generation remain current before using the result.
    pub fn resolve_dapp_session_account(
        &self,
        view_session: &DesktopViewSession,
        public_account_uuid: &str,
        public_account_scope: &PublicAccountScope,
        owning_private_wallet_uuid: Option<&str>,
    ) -> Result<WalletConnectSessionAccountResolution, VaultError> {
        let key = public_account_metadata_record_key(public_account_uuid);
        let Some(record) = self.encrypted_record_optional(&key)? else {
            return Ok(WalletConnectSessionAccountResolution::InvalidPublicAccount);
        };
        let mut account = view_session
            .view
            .decrypt_public_account_metadata(public_account_uuid, &record)?;
        if account.public_account_uuid != public_account_uuid {
            public_account_uuid.clone_into(&mut account.public_account_uuid);
        }

        if account.status != PublicAccountStatus::Active || &account.scope != public_account_scope {
            return Ok(WalletConnectSessionAccountResolution::InvalidPublicAccount);
        }

        match &account.scope {
            PublicAccountScope::Global => {
                Ok(WalletConnectSessionAccountResolution::Usable(account))
            }
            PublicAccountScope::PrivateWallet { wallet_uuid } => {
                if owning_private_wallet_uuid != Some(wallet_uuid.as_str()) {
                    return Ok(WalletConnectSessionAccountResolution::InvalidPublicAccount);
                }
                if wallet_uuid == view_session.wallet_id() {
                    Ok(WalletConnectSessionAccountResolution::Usable(account))
                } else {
                    Ok(
                        WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet {
                            owning_wallet_uuid: wallet_uuid.clone(),
                        },
                    )
                }
            }
        }
    }

    pub fn reconcile_walletconnect_session_account_state(
        &self,
        view_session: &DesktopViewSession,
        session_uuid: &str,
    ) -> Result<WalletConnectSessionRecord, VaultError> {
        let mut session = self.load_walletconnect_session(view_session, session_uuid)?;
        session.lifecycle_state = match session.lifecycle_state {
            WalletConnectSessionLifecycleState::Invalid
            | WalletConnectSessionLifecycleState::Disconnected
            | WalletConnectSessionLifecycleState::Expired => session.lifecycle_state,
            WalletConnectSessionLifecycleState::Active
            | WalletConnectSessionLifecycleState::TemporarilyPaused => match self
                .resolve_walletconnect_session_account(view_session, &session)?
            {
                WalletConnectSessionAccountResolution::Usable(_) => {
                    WalletConnectSessionLifecycleState::Active
                }
                WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet {
                    ..
                } => WalletConnectSessionLifecycleState::TemporarilyPaused,
                WalletConnectSessionAccountResolution::InvalidPublicAccount => {
                    WalletConnectSessionLifecycleState::Invalid
                }
            },
        };
        self.update_walletconnect_session(view_session, &session)?;
        Ok(session)
    }
}
