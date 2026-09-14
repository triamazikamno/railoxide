//! Gateway grants use an optional encrypted namespace with the existing vault view key.
//! There are no released rows to migrate or rewrite; a missing namespace is empty.
//! Unknown payload versions fail closed, and no plaintext permission index is stored.

use serde::{Deserialize, Serialize};

use super::{
    DesktopVaultStore, DesktopViewSession, EncryptedRecord, PublicAccountScope, RecordKind,
    VaultError, WalletConnectSessionAccountResolution, Zeroizing, generate_opaque_id,
};
use crate::RpcOrigin;

const GATEWAY_PERMISSION_PREFIX: &str = "gateway-permission|";

/// Account disclosure permission for an authenticated peer and web origin.
#[derive(Clone, PartialEq, Eq)]
pub struct GatewayPermission {
    pub permission_id: String,
    pub origin: RpcOrigin,
    pub public_account_uuid: String,
    pub public_account_scope: PublicAccountScope,
    pub owning_private_wallet_uuid: Option<String>,
    pub chain_id: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredGatewayPermission {
    version: u32,
    permission_id: String,
    paired_peer_id: String,
    url: String,
    public_account_uuid: String,
    public_account_scope: PublicAccountScope,
    owning_private_wallet_uuid: Option<String>,
    chain_id: u64,
}

fn decode_permission(
    view_session: &DesktopViewSession,
    permission_id: &str,
    record: &EncryptedRecord,
) -> Result<GatewayPermission, VaultError> {
    let plaintext =
        view_session
            .view
            .decrypt_record(RecordKind::GatewayPermission, permission_id, record)?;
    let stored: StoredGatewayPermission = rmp_serde::from_slice(&plaintext)?;
    if stored.version != 1 || stored.permission_id != permission_id {
        return Err(VaultError::InvalidGatewayPermission);
    }
    let origin = RpcOrigin::dapp(stored.paired_peer_id, &stored.url)
        .map_err(|_| VaultError::InvalidGatewayPermission)?;
    Ok(GatewayPermission {
        permission_id: stored.permission_id,
        origin,
        public_account_uuid: stored.public_account_uuid,
        public_account_scope: stored.public_account_scope,
        owning_private_wallet_uuid: stored.owning_private_wallet_uuid,
        chain_id: stored.chain_id,
    })
}

impl DesktopVaultStore {
    pub fn list_gateway_permissions(
        &self,
        view_session: &DesktopViewSession,
    ) -> Result<Vec<GatewayPermission>, VaultError> {
        self.db
            .list_desktop_wallet_vault_records(GATEWAY_PERMISSION_PREFIX)?
            .into_iter()
            .map(|stored| {
                let permission_id = stored
                    .key
                    .strip_prefix(GATEWAY_PERMISSION_PREFIX)
                    .ok_or(VaultError::InvalidGatewayPermission)?;
                let record: EncryptedRecord = rmp_serde::from_slice(&stored.payload)?;
                decode_permission(view_session, permission_id, &record)
            })
            .collect()
    }

    /// The caller supplies a peer-authenticated, browser-attested origin and owns
    /// unlock lifecycle checks and chain policy before committing this grant.
    pub fn grant_gateway_permission(
        &self,
        view_session: &DesktopViewSession,
        origin: &RpcOrigin,
        public_account_uuid: &str,
        chain_id: u64,
    ) -> Result<GatewayPermission, VaultError> {
        let paired_peer_id = origin
            .paired_peer_id()
            .ok_or(VaultError::InvalidGatewayPermission)?;
        let url = origin
            .web_origin()
            .ok_or(VaultError::InvalidGatewayPermission)?;
        let origin = RpcOrigin::dapp_web_origin(paired_peer_id, url.as_str())
            .map_err(|_| VaultError::InvalidGatewayPermission)?;
        let url = origin
            .web_origin()
            .ok_or(VaultError::InvalidGatewayPermission)?;
        let account = self
            .list_public_account_metadata_with_view(&view_session.view)?
            .into_iter()
            .find(|account| account.public_account_uuid == public_account_uuid)
            .ok_or(VaultError::PublicAccountNotFound)?;
        let owning_private_wallet_uuid = match &account.scope {
            PublicAccountScope::Global => None,
            PublicAccountScope::PrivateWallet { wallet_uuid } => Some(wallet_uuid.clone()),
        };
        if !matches!(
            self.resolve_dapp_session_account(
                view_session,
                public_account_uuid,
                &account.scope,
                owning_private_wallet_uuid.as_deref(),
            )?,
            WalletConnectSessionAccountResolution::Usable(_)
        ) {
            return Err(VaultError::InvalidPublicAccountOperation);
        }
        let permission_id = match self
            .list_gateway_permissions(view_session)?
            .into_iter()
            .find(|permission| permission.origin == origin)
        {
            Some(permission) => permission.permission_id,
            None => generate_opaque_id()?,
        };
        let stored = StoredGatewayPermission {
            version: 1,
            permission_id: permission_id.clone(),
            paired_peer_id: paired_peer_id.to_owned(),
            url: url.as_str().to_owned(),
            public_account_uuid: account.public_account_uuid.clone(),
            public_account_scope: account.scope.clone(),
            owning_private_wallet_uuid: owning_private_wallet_uuid.clone(),
            chain_id,
        };
        let plaintext = Zeroizing::new(rmp_serde::to_vec_named(&stored)?);
        let record = view_session.view.encrypt_record(
            RecordKind::GatewayPermission,
            &permission_id,
            &plaintext,
        )?;
        self.db.put_desktop_wallet_vault_record(
            &format!("{GATEWAY_PERMISSION_PREFIX}{permission_id}"),
            &rmp_serde::to_vec_named(&record)?,
        )?;
        Ok(GatewayPermission {
            permission_id,
            origin,
            public_account_uuid: account.public_account_uuid,
            public_account_scope: account.scope,
            owning_private_wallet_uuid,
            chain_id,
        })
    }

    pub fn delete_gateway_permission(
        &self,
        view_session: &DesktopViewSession,
        permission_id: &str,
    ) -> Result<(), VaultError> {
        let key = format!("{GATEWAY_PERMISSION_PREFIX}{permission_id}");
        let record = self.encrypted_record(&key)?;
        decode_permission(view_session, permission_id, &record)?;
        self.db.delete_desktop_wallet_vault_record(&key)?;
        Ok(())
    }
}
