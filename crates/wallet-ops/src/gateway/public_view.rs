//! Privileged frontend operations reuse the provider's permission and document owners.
use super::{DappProvider, GatewayError, Instant, PeerId};
use crate::gateway::GatewayPublicCommand;

impl DappProvider {
    pub(in crate::gateway) fn attach_ui_peer(&mut self, session: u64, peer: PeerId) {
        self.ui_peers.insert(session, peer);
        self.push_ui(session);
    }

    pub(in crate::gateway) fn public_command(
        &mut self,
        session: u64,
        peer: PeerId,
        generation: u64,
        command: GatewayPublicCommand,
    ) -> Option<GatewayPublicCommand> {
        if generation != self.generation
            || self.wallet.view.is_none()
            || !self.authority.borrow().same_authority(&self.wallet)
        {
            return None;
        }
        self.ui_errors.remove(&session);
        let result = match &command {
            GatewayPublicCommand::SelectAccount {
                public_account_uuid,
            } => {
                if self.wallet.public_accounts.iter().any(|account| {
                    account.public_account_uuid == *public_account_uuid
                        && account.is_active_for_wallet(
                            self.wallet.view.as_ref().expect("unlocked").wallet_id(),
                        )
                }) {
                    return Some(command);
                }
                Err(GatewayError::Unavailable)
            }
            GatewayPublicCommand::SelectChain { chain_id } => {
                if self.wallet.chain_ids.contains(chain_id) {
                    return Some(command);
                }
                Err(GatewayError::Unavailable)
            }
            GatewayPublicCommand::Draft { .. } | GatewayPublicCommand::RefreshBalances => {
                return Some(command);
            }
            GatewayPublicCommand::RevokePermission { permission_id }
            | GatewayPublicCommand::ReissuePermission { permission_id, .. } => {
                self.edit_peer_permission(peer, permission_id, &command)
            }
            GatewayPublicCommand::ConnectTab { document } => {
                let peer_id = alloy::hex::encode(peer.to_bytes());
                if self
                    .documents
                    .get(&(session, document.clone()))
                    .is_some_and(|doc| doc.origin.paired_peer_id() == Some(peer_id.as_str()))
                {
                    let mut id = [0; 16];
                    if getrandom::fill(&mut id).is_ok() {
                        self.request(
                            session,
                            document.clone(),
                            format!("ui-{}", alloy::hex::encode(id)),
                            "eth_requestAccounts",
                            serde_json::json!([]),
                            Instant::now(),
                        )
                    } else {
                        Err(GatewayError::Unavailable)
                    }
                } else {
                    Err(GatewayError::Unavailable)
                }
            }
        };
        if result.is_err() {
            self.ui_errors.insert(
                session,
                "Could not update site access. Refresh and try again.".to_owned(),
            );
        }
        self.push_pending_ui();
        self.push_ui(session);
        None
    }

    fn edit_peer_permission(
        &mut self,
        peer: PeerId,
        permission_id: &str,
        command: &GatewayPublicCommand,
    ) -> Result<(), GatewayError> {
        self.reload_permissions();
        let peer_id = alloy::hex::encode(peer.to_bytes());
        let permission = self
            .permissions
            .iter()
            .find(|permission| {
                permission.permission_id == permission_id
                    && permission.origin.paired_peer_id() == Some(peer_id.as_str())
            })
            .ok_or(GatewayError::Unavailable)?
            .clone();
        if let GatewayPublicCommand::ReissuePermission {
            public_account_uuid,
            ..
        } = command
        {
            if !self.wallet.chain_ids.contains(&permission.chain_id) {
                return Err(GatewayError::Unavailable);
            }
            self.store
                .grant_gateway_permission(
                    self.wallet.view.as_ref().ok_or(GatewayError::Unavailable)?,
                    &permission.origin,
                    public_account_uuid,
                    permission.chain_id,
                )
                .map_err(|_| GatewayError::Storage)?;
            self.reload_permissions();
            self.refresh_documents(false);
            self.refresh_approvals(Instant::now());
            self.invalidate_reads(Instant::now());
            Ok(())
        } else {
            self.revoke(permission_id)
        }
    }
}
